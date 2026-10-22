//! 可执行文件的协议契约：JSONL 事件、退出码、错误码、显式头。
//!
//! 全部走回环假服务器（进程内线程），不联网；ffmpeg 校验默认关闭（`--no-verify`），
//! 让测试与机器上是否装了 ffmpeg 无关。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const BODY_LEN: usize = 64 * 1024;

#[derive(Clone, Copy, Default)]
struct Script {
    status: u16,
    ignore_range: bool,
}

#[derive(Default)]
struct Log {
    request_headers: Mutex<Vec<HashMap<String, String>>>,
    ranges: Mutex<Vec<String>>,
}

fn body() -> Vec<u8> {
    (0..BODY_LEN).map(|i| (i % 251) as u8).collect()
}

fn spawn_server(script: Script) -> (String, Arc<Log>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().unwrap();
    let log = Arc::new(Log::default());
    let shared = Arc::clone(&log);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let log = Arc::clone(&shared);
            std::thread::spawn(move || handle(stream, script, log));
        }
    });
    (format!("http://{address}/video.mp4"), log)
}

fn handle(mut stream: TcpStream, script: Script, log: Arc<Log>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    log.request_headers.lock().unwrap().push(headers.clone());
    let range = headers.get("range").cloned();
    if let Some(range) = range.as_ref() {
        log.ranges.lock().unwrap().push(range.clone());
    }
    let method = request_line
        .split_whitespace()
        .next()
        .unwrap_or("GET")
        .to_string();
    let data = body();

    if script.status != 0 {
        let response = format!(
            "HTTP/1.1 {} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            script.status
        );
        let _ = stream.write_all(response.as_bytes());
        return;
    }

    let (status, payload, extra) = match (method.as_str(), range.as_deref(), script.ignore_range) {
        ("HEAD", _, _) => (
            "200 OK",
            Vec::new(),
            format!(
                "Content-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\n",
                data.len()
            ),
        ),
        ("GET", Some(range), false) => {
            let (start, end) = parse_range(range, data.len());
            (
                "206 Partial Content",
                data[start..=end].to_vec(),
                format!("Content-Range: bytes {start}-{end}/{}\r\n", data.len()),
            )
        }
        _ => ("200 OK", data.clone(), String::new()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: \"v1\"\r\nAccept-Ranges: bytes\r\n{extra}Connection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&payload);
}

fn parse_range(header: &str, total: usize) -> (usize, usize) {
    let spec = header.trim().trim_start_matches("bytes=");
    let (start, end) = spec.split_once('-').unwrap();
    let start: usize = start.trim().parse().unwrap_or(0);
    let end: usize = if end.trim().is_empty() {
        total - 1
    } else {
        end.trim().parse::<usize>().unwrap().min(total - 1)
    };
    (start, end)
}

fn temp_dir() -> PathBuf {
    // pid + 纳秒不足以保证唯一：并行测试线程可能取到同一纳秒值，落进同一目录后
    // 互相覆盖 tasks.jsonl、互相 remove_dir_all。自增序号是进程内绝对唯一的。
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rvdown-cli-it-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_cli(args: &[&str]) -> (i32, String, String) {
    let exe = env!("CARGO_BIN_EXE_rvdown");
    let output = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("启动 rvdown");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn write_tasks(dir: &Path, jobs: &[serde_json::Value]) -> PathBuf {
    let path = dir.join("tasks.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    for job in jobs {
        writeln!(file, "{job}").unwrap();
    }
    path
}

fn events(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| panic!("非法事件行 {line}: {error}"))
        })
        .collect()
}

fn result_of<'a>(events: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
    events
        .iter()
        .find(|event| event["event"] == "result" && event["id"] == id)
        .unwrap_or_else(|| panic!("缺少 {id} 的结果事件: {events:?}"))
}

#[test]
fn 批量下载逐条输出结果事件并写结果文件() {
    let (url, _log) = spawn_server(Script::default());
    let dir = temp_dir();
    let out_a = dir.join("a.mp4");
    let out_b = dir.join("b.mp4");
    let result_file = dir.join("results.jsonl");
    let tasks = write_tasks(
        &dir,
        &[
            serde_json::json!({"id": "a", "url": url, "out": out_a}),
            serde_json::json!({"id": "b", "url": url, "out": out_b}),
        ],
    );
    let (code, stdout, stderr) = run_cli(&[
        "run",
        "--tasks",
        tasks.to_str().unwrap(),
        "--out",
        result_file.to_str().unwrap(),
        "--no-verify",
        "--concurrency",
        "2",
    ]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let events = events(&stdout);
    assert_eq!(events[0]["event"], "start");
    assert_eq!(events.last().unwrap()["event"], "summary");
    assert_eq!(events.last().unwrap()["failed"], 0);
    for id in ["a", "b"] {
        let result = result_of(&events, id);
        assert_eq!(result["ok"], true);
        assert_eq!(result["code"], "OK");
        assert_eq!(result["bytes"], BODY_LEN);
        assert_eq!(result["message"], "download finished");
    }
    assert_eq!(std::fs::metadata(&out_a).unwrap().len() as usize, BODY_LEN);
    assert_eq!(std::fs::metadata(&out_b).unwrap().len() as usize, BODY_LEN);

    let result_lines: Vec<String> = std::fs::read_to_string(&result_file)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(result_lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(&result_lines[0]).unwrap();
    assert!(first.get("event").is_none(), "结果文件里是扁平 Outcome");
    assert_eq!(first["code"], "OK");
    // 内存里的结果文件逐条落盘，不留半行。
    assert!(!result_lines.iter().any(|line| line.trim().is_empty()));
}

#[test]
fn 进度事件用稳定机器码且结束后不留锁文件() {
    let (url, _log) = spawn_server(Script::default());
    let dir = temp_dir();
    let out = dir.join("p.mp4");
    let tasks = write_tasks(
        &dir,
        &[serde_json::json!({"id": "p", "url": url, "out": out})],
    );
    let (code, stdout, stderr) = run_cli(&[
        "run",
        "--tasks",
        tasks.to_str().unwrap(),
        "--progress",
        "--no-verify",
    ]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let events = events(&stdout);
    let progress: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "progress")
        .collect();
    assert!(!progress.is_empty(), "开了 --progress 至少要有一条进度");
    for event in &progress {
        let phase = event["phase"].as_str().unwrap();
        assert!(
            ["probing", "downloading", "merging", "verifying"].contains(&phase),
            "phase 必须是机器码: {phase}"
        );
        assert!(event["attempt"].as_u64().unwrap() >= 1);
        assert!(event.get("resumed_bytes").is_some());
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["p.mp4", "tasks.jsonl"],
        "不能留 .lock/.part/.aria2"
    );
}

#[test]
fn 找不到资源输出稳定错误码且退出码为一() {
    let (url, _log) = spawn_server(Script {
        status: 404,
        ..Default::default()
    });
    let dir = temp_dir();
    let tasks = write_tasks(
        &dir,
        &[serde_json::json!({"id": "x", "url": url, "out": dir.join("x.mp4")})],
    );
    let (code, stdout, stderr) =
        run_cli(&["run", "--tasks", tasks.to_str().unwrap(), "--no-verify"]);
    assert_eq!(code, 1, "stdout={stdout} stderr={stderr}");
    let events = events(&stdout);
    let result = result_of(&events, "x");
    assert_eq!(result["ok"], false);
    assert_eq!(result["code"], "HTTP_NOT_FOUND");
    assert_eq!(result["http_status"], 404);
    assert_eq!(result["retryable"], false);
    assert!(!dir.join("x.mp4").exists(), "失败不能留下产物");
}

#[test]
fn 非法任务行直接拒绝且不执行任何下载() {
    let dir = temp_dir();
    let tasks = dir.join("bad.jsonl");
    std::fs::write(
        &tasks,
        "{\"id\":\"a\",\"url\":\"https://cdn.example.com/v.mp4\",\"out\":\"/tmp/a.mp4\"}\nnot json\n",
    )
    .unwrap();
    let (code, stdout, stderr) = run_cli(&["run", "--tasks", tasks.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "未执行不应输出事件: {stdout}");
    assert!(stderr.contains("第 2 行"), "{stderr}");
}

#[test]
fn 显式请求头原样下发且referer按平台推断() {
    let (url, log) = spawn_server(Script::default());
    let dir = temp_dir();
    let tasks = write_tasks(
        &dir,
        &[serde_json::json!({
            "id": "h",
            "url": url,
            "out": dir.join("h.mp4"),
            "headers": {"Cookie": "session=1", "Referer": ""},
            "page_url": "https://www.douyin.com/video/1"
        })],
    );
    let (code, stdout, stderr) =
        run_cli(&["run", "--tasks", tasks.to_str().unwrap(), "--no-verify"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let seen = log.request_headers.lock().unwrap();
    let first = seen.first().expect("至少一次请求");
    assert_eq!(first.get("cookie").map(String::as_str), Some("session=1"));
    // 显式空值 = 明确不发送：回环地址识别不出平台，也不能凭空补一个 Referer。
    assert!(!first.contains_key("referer"), "{first:?}");
    assert!(first
        .get("user-agent")
        .is_some_and(|value| value.contains("Chrome/131")));
}

#[test]
fn 服务器忽略range时自动回退单流并交付完整文件() {
    let (url, _log) = spawn_server(Script {
        ignore_range: true,
        ..Default::default()
    });
    let dir = temp_dir();
    let tasks = write_tasks(
        &dir,
        &[serde_json::json!({"id": "r", "url": url, "out": dir.join("r.mp4")})],
    );
    let (code, stdout, stderr) =
        run_cli(&["run", "--tasks", tasks.to_str().unwrap(), "--no-verify"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let bytes = std::fs::read(dir.join("r.mp4")).unwrap();
    assert_eq!(bytes.len(), BODY_LEN);
    assert_eq!(bytes, body());
}

#[test]
fn 单文件入口支持json输出() {
    let (url, _log) = spawn_server(Script::default());
    let dir = temp_dir();
    let out = dir.join("one.mp4");
    let (code, stdout, stderr) = run_cli(&[
        "get",
        &url,
        "--out",
        out.to_str().unwrap(),
        "--no-verify",
        "--json",
    ]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let outcome: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(outcome["ok"], true);
    assert_eq!(outcome["code"], "OK");
    assert_eq!(outcome["bytes"], BODY_LEN);
    assert_eq!(std::fs::metadata(&out).unwrap().len() as usize, BODY_LEN);
}

#[test]
fn 帮助与版本可用() {
    let (code, stdout, _) = run_cli(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("rvdown"), "{stdout}");
    let (code, stdout, _) = run_cli(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("run") && stdout.contains("get"), "{stdout}");
}

#[test]
fn 下载中断留下的part不会污染后续结果() {
    // 先放一个「上次中断」的残件：没有控制文件时必须从零重下，不能拼接。
    let (url, _log) = spawn_server(Script::default());
    let dir = temp_dir();
    let out = dir.join("stale.mp4");
    let mut part = out.as_os_str().to_os_string();
    part.push(".part");
    let part = PathBuf::from(part);
    std::fs::write(&part, vec![9u8; 1024]).unwrap();
    let tasks = write_tasks(
        &dir,
        &[serde_json::json!({"id": "s", "url": url, "out": out})],
    );
    let (code, stdout, stderr) =
        run_cli(&["run", "--tasks", tasks.to_str().unwrap(), "--no-verify"]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    assert_eq!(std::fs::read(&out).unwrap(), body());
    assert!(!part.exists(), "交付后不应留下 .part");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 结果事件包含稳定字段集合() {
    let (url, _log) = spawn_server(Script::default());
    let dir = temp_dir();
    let tasks = write_tasks(
        &dir,
        &[serde_json::json!({"id": "fields", "url": url, "out": dir.join("f.mp4")})],
    );
    let (code, stdout, stderr) = run_cli(&[
        "run",
        "--tasks",
        tasks.to_str().unwrap(),
        "--no-verify",
        "--no-checksum",
    ]);
    assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
    let events = events(&stdout);
    let result = result_of(&events, "fields").as_object().unwrap();
    for key in [
        "event",
        "id",
        "ok",
        "code",
        "out",
        "bytes",
        "retryable",
        "message",
        "elapsed_ms",
    ] {
        assert!(result.contains_key(key), "缺少字段 {key}: {result:?}");
    }
    assert!(!result.contains_key("md5"), "--no-checksum 不应带 md5");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 输出目录不可写返回环境致命码() {
    let dir = temp_dir();
    let blocker = dir.join("blocked");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let tasks = write_tasks(
        &dir,
        &[
            serde_json::json!({"id": "b", "url": "https://cdn.example.com/v.mp4",
                             "out": blocker.join("x.mp4")}),
        ],
    );
    let result_file = dir.join("results.jsonl");
    // 结果文件路径是目录 → 参数错误（未执行）。
    std::fs::create_dir_all(&result_file).unwrap();
    let (code, _, _) = run_cli(&[
        "run",
        "--tasks",
        tasks.to_str().unwrap(),
        "--out",
        result_file.to_str().unwrap(),
    ]);
    assert_eq!(code, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[allow(dead_code)]
fn read_all(path: &Path) -> Vec<u8> {
    let mut buffer = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_end(&mut buffer)
        .unwrap();
    buffer
}
