//! 公开样本诊断：同一浏览器传输栈的 HEAD→GET 与直接 GET，不改变库默认策略。
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    error::Error,
    path::PathBuf,
    time::{Duration, Instant},
};
use wreq::{Client, Response};
use wreq_util::Profile;

type Failure = Box<dyn Error + Send + Sync>;
fn with_head(mode: &str) -> Option<bool> {
    match mode {
        "get" => Some(false),
        "head-get" => Some(true),
        _ => None,
    }
}
fn public_target(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("interactive-examples.mdn.mozilla.net")
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    })
}
fn loopback_proxy(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|u| {
        u.scheme() == "http"
            && u.host_str() == Some("127.0.0.1")
            && u.port().is_some()
            && u.username().is_empty()
            && u.password().is_none()
    })
}
fn metadata(response: &Response, kind: &str, started: Instant) -> Value {
    json!({"method":kind,"status":response.status().as_u16(),"http_version":format!("{:?}",response.version()),"headers_at_s":started.elapsed().as_secs_f64()})
}
async fn transfer(
    client: &Client,
    url: &str,
    head: bool,
    started: Instant,
    responses: &mut Vec<Value>,
    bytes: &mut Vec<u8>,
) -> Result<(), Failure> {
    if head {
        let response = client
            .head(url)
            .header("User-Agent", "Rvdown-Benchmark/1.0")
            .header("Accept-Encoding", "identity")
            .send()
            .await?;
        responses.push(metadata(&response, "HEAD", started));
        if response.status() != 200 {
            return Err("HEAD is not 200".into());
        }
        drop(response);
    }
    let response = client
        .get(url)
        .header("User-Agent", "Rvdown-Benchmark/1.0")
        .header("Accept-Encoding", "identity")
        .send()
        .await?;
    responses.push(metadata(&response, "GET", started));
    if response.status() != 200 {
        return Err("GET is not 200".into());
    }
    let index = responses.len() - 1;
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if bytes.is_empty() {
            responses[index]["first_body_at_s"] = json!(started.elapsed().as_secs_f64());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(())
}
#[tokio::main]
async fn main() -> Result<(), Failure> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 5 || !public_target(&args[1]) || !loopback_proxy(&args[2]) {
        return Err("expected public MDN URL, loopback proxy, head-get|get, output".into());
    }
    let head = with_head(&args[3]).ok_or("unknown request mode")?;
    let client = Client::builder()
        .emulation(Profile::Chrome131)
        .proxy(wreq::Proxy::all(&args[2])?)
        .http2_only()
        .redirect(wreq::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()?;
    let started = Instant::now();
    let mut responses = Vec::new();
    let mut bytes = Vec::new();
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        transfer(&client, &args[1], head, started, &mut responses, &mut bytes),
    )
    .await;
    let elapsed = started.elapsed().as_secs_f64();
    let outcome = match result {
        Ok(Ok(())) => "complete",
        Ok(Err(error)) => {
            eprintln!("{error}");
            "transport_error"
        }
        Err(_) => "diagnostic_deadline",
    };
    if outcome == "complete" {
        let out = PathBuf::from(&args[4]);
        let tmp = out.with_extension("part");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(tmp, out)?;
    }
    println!(
        "{}",
        json!({"mode":args[3],"outcome":outcome,"transport_elapsed_s":elapsed,"bytes":bytes.len(),"responses":responses,"scope":"controlled HTTP2 transport experiment; in-memory body; final write/hash excluded from transport timer"})
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 诊断必须区分一次get与head后get() {
        assert_eq!(with_head("get"), Some(false));
        assert_eq!(with_head("head-get"), Some(true));
        assert_eq!(with_head("other"), None);
    }
    #[test]
    fn 诊断不能接收凭据地址或外部代理() {
        assert!(public_target(
            "https://interactive-examples.mdn.mozilla.net/a.mp4"
        ));
        assert!(!public_target(
            "https://interactive-examples.mdn.mozilla.net/a?secret=x"
        ));
        assert!(!loopback_proxy("http://example.com:7890"));
        assert!(loopback_proxy("http://127.0.0.1:12345"));
    }
}
