//! 实际子进程与回环服务器：持续滴流不触发空闲超时，低速策略必须切换候选且不拼错内容。
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn body(slow: bool) -> Vec<u8> {
    vec![if slow { 13 } else { 37 }; 128 * 1024]
}
fn server() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let slow = Arc::new(AtomicUsize::new(0));
    let fast = Arc::new(AtomicUsize::new(0));
    let counters = (slow.clone(), fast.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let counters = (counters.0.clone(), counters.1.clone());
            std::thread::spawn(move || handle(stream, counters));
        }
    });
    (format!("http://{address}"), slow, fast)
}
fn handle(mut stream: TcpStream, counters: (Arc<AtomicUsize>, Arc<AtomicUsize>)) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
    }
    let slow = request.contains("/slow ");
    let data = body(slow);
    let etag = if slow { "slow-object" } else { "fast-object" };
    if write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"{etag}\"\r\nConnection: close\r\n\r\n",data.len()).is_err(){return;}
    if request.starts_with("HEAD ") {
        return;
    }
    if slow {
        counters.0.fetch_add(1, Ordering::Relaxed);
    } else {
        counters.1.fetch_add(1, Ordering::Relaxed);
    }
    for chunk in data.chunks(1024) {
        if stream
            .write_all(chunk)
            .and_then(|_| stream.flush())
            .is_err()
        {
            break;
        }
        if slow {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn 持续滴流时及时切换备用且完整文件只来自成功候选() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.bin");
    let (origin, slow, fast) = server();
    let started = Instant::now();
    let result = Command::new(env!("CARGO_BIN_EXE_rvdown"))
        .args([
            "get",
            &format!("{origin}/slow"),
            "--fallback",
            &format!("{origin}/fast"),
            "--out",
            dest.to_str().unwrap(),
            "--json",
            "--no-proxy",
            "--no-verify",
            "--no-checksum",
            "--connections",
            "1",
            "--retry-attempts",
            "3",
            "--low-speed-limit",
            "65536",
            "--low-speed-window",
            "1",
        ])
        .output()
        .unwrap();
    let output = String::from_utf8(result.stdout).unwrap();
    assert!(result.status.success(), "{output}");
    let outcome: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(outcome["source_url"], format!("{origin}/fast"));
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        body(false),
        "不能把旧候选的前缀拼到新资源"
    );
    assert_eq!(
        slow.load(Ordering::Relaxed),
        1,
        "持续低速直接试备用，不能先重试慢源三遍"
    );
    assert_eq!(fast.load(Ordering::Relaxed), 1);
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "滴流完整接收约6.4秒，保护应更早切换"
    );
}

#[test]
fn 没有备用时低速返回独立错误码且绝不交付部分文件() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out.bin");
    let (origin, _, _) = server();
    let result = Command::new(env!("CARGO_BIN_EXE_rvdown"))
        .args([
            "get",
            &format!("{origin}/slow"),
            "--out",
            dest.to_str().unwrap(),
            "--json",
            "--no-proxy",
            "--no-verify",
            "--no-checksum",
            "--connections",
            "1",
            "--low-speed-limit",
            "65536",
            "--low-speed-window",
            "1",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let outcome: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(outcome["code"], "NETWORK_TOO_SLOW");
    assert_eq!(outcome["retryable"], true);
    assert!(!dest.exists(), "取消的部分文件不能作为完整交付");
}
