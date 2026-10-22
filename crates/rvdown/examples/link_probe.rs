//! 公开直链诊断：只请求前 256 KiB，区分响应头/正文延迟；不输出 URL、请求头或配置。
use futures::StreamExt;
use md5::{Digest, Md5};
use rvdown::transport::{plan::SegmentRange, segment::SegmentDownloader};
use rvdown::{
    DohJsonResolver, TransportConfig, WreqSegmentDownloader, DEFAULT_DOWNLOAD_USER_AGENT,
};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: link_probe HTTPS_URL PHYSICAL_INTERFACE browser|benchmark".into());
    }
    let url = url::Url::parse(&args[1])?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
    {
        return Err("diagnostic requires credential-free HTTPS without query".into());
    }
    let ua = match args[3].as_str() {
        "browser" => DEFAULT_DOWNLOAD_USER_AGENT,
        "benchmark" => "Rvdown-Benchmark/1.0",
        _ => return Err("unknown UA mode".into()),
    };
    let config = TransportConfig {
        interface: Some(args[2].clone()),
        no_proxy: true,
        dns: Some(Arc::new(DohJsonResolver::new(
            "https://dns.alidns.com/resolve",
            "dns.alidns.com",
            "223.5.5.5".parse()?,
            Some(args[2].clone()),
        ))),
        ..Default::default()
    };
    let client = WreqSegmentDownloader::with_config(&config, None)?;
    let headers = HashMap::from([
        ("User-Agent".into(), ua.into()),
        ("Accept-Encoding".into(), "identity".into()),
    ]);
    let started = Instant::now();
    let mut header_s = None;
    let mut first_byte_s = None;
    let mut bytes = 0u64;
    let mut status = None;
    let mut hash = Md5::new();
    let transfer = async {
        let response = client
            .get(
                url.as_str(),
                Some(&SegmentRange {
                    index: 0,
                    start: 0,
                    end: 262143,
                }),
                &headers,
            )
            .await?;
        header_s = Some(started.elapsed().as_secs_f64());
        status = Some(response.status);
        let mut stream = response.body;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            first_byte_s.get_or_insert_with(|| started.elapsed().as_secs_f64());
            bytes += chunk.len() as u64;
            hash.update(&chunk);
        }
        Ok::<_, rvdown::transport::segment::TransportError>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(40), transfer).await;
    let outcome = match result {
        Ok(Ok(())) => "complete",
        Ok(Err(_)) => "transport_error",
        Err(_) => "diagnostic_deadline",
    };
    println!(
        "{}",
        serde_json::json!({"ua_profile":args[3],"status":status,"time_to_headers_s":header_s,
        "time_to_first_body_byte_s":first_byte_s,"elapsed_s":started.elapsed().as_secs_f64(),
        "bytes":bytes,"prefix_md5":format!("{:x}",hash.finalize()),"outcome":outcome,
        "scope":"first 256 KiB only; native Chrome131 TLS transport; not a complete-file speed result"})
    );
    Ok(())
}
