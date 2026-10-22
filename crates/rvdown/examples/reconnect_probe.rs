//! 受控断开/重连实验；不改下载器默认策略。重连字节必须确认同一资源。

use futures::StreamExt;
use rvdown::transport::identity::{ContentRange, ResourceIdentity};
use serde_json::{json, Value};
use std::{
    error::Error,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use wreq::{Client, Response};
use wreq_util::Profile;

type Failure = Box<dyn Error + Send + Sync>;
const CUT: usize = 262144;

struct PinnedDns {
    host: String,
    address: SocketAddr,
}
impl wreq::dns::Resolve for PinnedDns {
    fn resolve(&self, name: wreq::dns::Name) -> wreq::dns::Resolving {
        let valid = name.as_str().eq_ignore_ascii_case(&self.host);
        let address = self.address;
        Box::pin(async move {
            if !valid {
                return Err(Box::new(std::io::Error::other("unexpected DNS name")) as Failure);
            }
            Ok(Box::new(std::iter::once(address)) as wreq::dns::Addrs)
        })
    }
}

#[derive(Clone, Copy)]
enum Flavor {
    Original,
    Chrome149,
    Firefox144,
    Http1,
}
impl Flavor {
    fn parse(arm: &str) -> Result<Self, Failure> {
        match arm {
            "continue" | "reconnect-same" => Ok(Self::Original),
            "reconnect-chrome149" => Ok(Self::Chrome149),
            "reconnect-firefox144" => Ok(Self::Firefox144),
            "reconnect-http1" => Ok(Self::Http1),
            _ => Err("unknown experiment arm".into()),
        }
    }
    fn profile(self) -> Profile {
        match self {
            Self::Chrome149 => Profile::Chrome149,
            Self::Firefox144 => Profile::Firefox144,
            _ => Profile::Chrome131,
        }
    }
    fn ua(self) -> &'static str {
        match self {
            Self::Chrome149=>"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36",
            Self::Firefox144=>"Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:144.0) Gecko/20100101 Firefox/144.0",
            _=>rvdown::DEFAULT_DOWNLOAD_USER_AGENT,
        }
    }
}

fn client(host: &str, address: IpAddr, interface: &str, flavor: Flavor) -> Result<Client, Failure> {
    let builder = Client::builder()
        .emulation(flavor.profile())
        .no_proxy()
        .redirect(wreq::redirect::Policy::none())
        .dns_resolver(Arc::new(PinnedDns {
            host: host.into(),
            address: SocketAddr::new(address, 443),
        }))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30));
    let builder = match flavor {
        Flavor::Http1 => builder.http1_only(),
        _ => builder.http2_only(),
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        Ok(builder.interface(interface.to_owned()).build()?)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (builder, interface);
        Err("this experiment requires macOS/Linux physical interface binding".into())
    }
}

fn header(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}
fn identity(response: &Response, total: u64) -> ResourceIdentity {
    ResourceIdentity::new(
        header(response, "etag"),
        header(response, "last-modified"),
        total,
    )
}
fn resume_matches(
    status: u16,
    old: &ResourceIdentity,
    current: &ResourceIdentity,
    range: Option<&str>,
    offset: u64,
) -> bool {
    status == 206
        && current.confirms_same_resource(old)
        && matches!(range.and_then(ContentRange::parse),Some(ContentRange::Satisfied{start,end,total:Some(total)})
            if start==offset && total==old.length && end.checked_add(1)==Some(total))
}
fn metadata(response: &Response, attempt: u8, started: Instant) -> Value {
    let server = header(response, "server")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let server_class = if server.contains("google") {
        "google"
    } else if server.contains("cloudflare") {
        "cloudflare"
    } else if server.contains("nginx") {
        "nginx"
    } else {
        "other"
    };
    let cache = header(response, "x-cache")
        .unwrap_or_default()
        .to_ascii_lowercase();
    json!({"attempt":attempt,"status":response.status().as_u16(),"http_version":format!("{:?}",response.version()),
        "headers_at_s":started.elapsed().as_secs_f64(),"content_length":header(response,"content-length").and_then(|v|v.parse::<u64>().ok()),
        "server_class":server_class,"cache_class":if cache.contains("hit"){"hit"}else if cache.contains("miss"){"miss"}else{"unknown"},
        "age_s":header(response,"age").and_then(|v|v.parse::<u64>().ok()),"via_present":response.headers().contains_key("via")})
}
fn progress(
    trace: &mut Vec<Value>,
    started: Instant,
    attempt: u8,
    total_bytes: u64,
    last: &mut Instant,
) {
    if last.elapsed() >= Duration::from_millis(500) {
        trace.push(json!({"attempt":attempt,"at_s":started.elapsed().as_secs_f64(),"application_bytes":total_bytes}));
        *last = Instant::now();
    }
}

#[derive(Clone, Copy)]
struct Attempt<'a> {
    url: &'a str,
    ip: IpAddr,
    interface: &'a str,
    arm: &'a str,
    started: Instant,
}

async fn transfer(
    input: &Attempt<'_>,
    trace: &mut Vec<Value>,
    responses: &mut Vec<Value>,
    counters: &mut Value,
) -> Result<Vec<u8>, Failure> {
    let Attempt {
        url,
        ip,
        interface,
        arm,
        started,
    } = *input;
    let host = url::Url::parse(url)?
        .host_str()
        .ok_or("missing host")?
        .to_owned();
    let flavor = Flavor::parse(arm)?;
    let mut data = Vec::new();
    let mut network = 0u64;
    let mut last = Instant::now();
    let original;
    {
        let first = client(&host, ip, interface, Flavor::Original)?;
        let response = first
            .get(url)
            .header("User-Agent", Flavor::Original.ua())
            .header("Accept-Encoding", "identity")
            .send()
            .await?;
        responses.push(metadata(&response, 1, started));
        if response.status() != 200 {
            return Err("initial response is not 200".into());
        }
        let total = header(&response, "content-length")
            .ok_or("missing total length")?
            .parse::<u64>()?;
        original = identity(&response, total);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if data.is_empty() {
                responses[0]["first_body_at_s"] = json!(started.elapsed().as_secs_f64());
            }
            network += chunk.len() as u64;
            let take = if arm == "continue" {
                chunk.len()
            } else {
                chunk.len().min(CUT.saturating_sub(data.len()))
            };
            data.extend_from_slice(&chunk[..take]);
            counters["application_network_bytes"] = json!(network);
            progress(trace, started, 1, network, &mut last);
            if arm != "continue" && data.len() >= CUT && (data.len() as u64) < total {
                break;
            }
        }
    } // 丢弃旧响应体和 Client，第二次必须新建连接，不能只在旧 HTTP2 流上改请求头。
    if data.len() as u64 == original.length {
        counters["interrupted"] = json!(false);
        return Ok(data);
    }
    if arm == "continue" {
        return Err("initial body incomplete".into());
    }
    counters["interrupted"] = json!(true);
    counters["cut_at_s"] = json!(started.elapsed().as_secs_f64());
    counters["retained_prefix_bytes"] = json!(data.len());
    let second = client(&host, ip, interface, flavor)?;
    let mut request = second
        .get(url)
        .header("User-Agent", flavor.ua())
        .header("Accept-Encoding", "identity");
    let resume = original.strong_etag().is_some();
    if let Some(etag) = original.strong_etag() {
        request = request
            .header(
                "Range",
                format!("bytes={}-{}", data.len(), original.length - 1),
            )
            .header("If-Range", etag);
    }
    let response = request.send().await?;
    responses.push(metadata(&response, 2, started));
    let status = response.status().as_u16();
    if resume && status == 206 {
        let current = identity(&response, original.length);
        if !resume_matches(
            status,
            &original,
            &current,
            header(&response, "content-range").as_deref(),
            data.len() as u64,
        ) {
            return Err("resume identity/range mismatch".into());
        }
        counters["resumed"] = json!(true);
    } else if status == 200 {
        data.clear();
        counters["resumed"] = json!(false);
    } else {
        return Err("unexpected reconnect response".into());
    }
    let expected = if status == 200 {
        header(&response, "content-length")
            .ok_or("missing reconnect length")?
            .parse::<u64>()?
    } else {
        original.length
    };
    let mut stream = response.bytes_stream();
    let mut first = true;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if first {
            responses[1]["first_body_at_s"] = json!(started.elapsed().as_secs_f64());
            first = false;
        }
        network += chunk.len() as u64;
        data.extend_from_slice(&chunk);
        counters["application_network_bytes"] = json!(network);
        progress(trace, started, 2, network, &mut last);
    }
    if data.len() as u64 != expected {
        return Err("final body length mismatch".into());
    }
    Ok(data)
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 6 {
        return Err("usage: reconnect_probe HTTPS_URL PUBLIC_IP INTERFACE ARM OUTPUT".into());
    }
    let url = url::Url::parse(&args[1])?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("public credential-free HTTPS required".into());
    }
    rvdown::net::validate_interface(&args[3])?;
    let ip: IpAddr = args[2].parse()?;
    Flavor::parse(&args[4])?;
    let started = Instant::now();
    let mut trace = Vec::new();
    let mut responses = Vec::new();
    let mut counters = json!({});
    let input = Attempt {
        url: url.as_str(),
        ip,
        interface: &args[3],
        arm: &args[4],
        started,
    };
    let task = transfer(&input, &mut trace, &mut responses, &mut counters);
    let result = tokio::time::timeout(Duration::from_secs(60), task).await;
    let mut output = json!({"arm":args[4],"elapsed_s":started.elapsed().as_secs_f64(),"responses":responses,
        "progress_trace":trace,"counters":counters,"forced_cut_bytes":CUT,"diagnostic_total_budget_s":60});
    match result {
        Ok(Ok(data)) => {
            let path = PathBuf::from(&args[5]);
            let temporary = path.with_extension("part");
            std::fs::write(&temporary, &data)?;
            std::fs::rename(&temporary, path)?;
            output["outcome"] = json!("complete");
            output["bytes"] = json!(data.len());
        }
        Ok(Err(error)) => {
            output["outcome"] = json!("transport_or_validation_error");
            // 原始错误只留本地 stderr；公开报告使用已筛选的字段。
            eprintln!("{error}");
        }
        Err(_) => output["outcome"] = json!("diagnostic_deadline"),
    }
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 切换指纹后仍必须有相同强etag和准确剩余区间才能续接() {
        let original = ResourceIdentity::new(Some("\"v1\"".into()), None, 1000);
        let same = ResourceIdentity::new(Some("\"v1\"".into()), None, 1000);
        assert!(resume_matches(
            206,
            &original,
            &same,
            Some("bytes 256-999/1000"),
            256
        ));
        for (status, current, range) in [
            (200, same.clone(), Some("bytes 256-999/1000")),
            (
                206,
                ResourceIdentity::new(Some("\"v2\"".into()), None, 1000),
                Some("bytes 256-999/1000"),
            ),
            (206, same.clone(), Some("bytes 0-743/1000")),
            (206, same.clone(), Some("bytes 256-999/2000")),
            (206, same, None),
        ] {
            assert!(!resume_matches(status, &original, &current, range, 256));
        }
    }
    #[test]
    fn 弱etag和同日期不足以确认跨连接续传身份() {
        for etag in [Some("W/\"v1\"".into()), None] {
            let old = ResourceIdentity::new(etag, Some("same date".into()), 1000);
            assert!(!resume_matches(
                206,
                &old,
                &old,
                Some("bytes 256-999/1000"),
                256
            ));
        }
    }
}
