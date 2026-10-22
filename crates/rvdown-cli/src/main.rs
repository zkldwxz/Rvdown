//! Rvdown 命令行：给任意语言调用的批量下载器。
//!
//! - `rvdown run --tasks <file|->`：JSON Lines 任务进，JSON Lines 事件出（stdout），
//!   `--out` 另写一份纯结果行；完成一条写一条并 flush，调用方可边下边读。
//! - `rvdown get <url> --out <path>`：单文件下载（进度走 stderr）。
//!
//! 退出码：`0` 全部成功；`1` 有任务失败（结果完整）；`2` 参数或任务文件错误（未执行）；
//! `3` 环境致命（配置无效、下载客户端初始化失败）；`130` 被中断（已完成部分保留）。

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use futures::{stream, StreamExt};
use rvdown::{
    Config, DohJsonResolver, ErrorCode, Job, Outcome, ProgressSink, TransferEvent, TransferPhase,
};

/// 进度事件节流窗口：同一任务/资产 500ms 最多一条。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Parser)]
#[command(
    name = "rvdown",
    version,
    about = "Fast multi-threaded video downloader (segmented Range, resume, TLS fingerprint, HLS/DASH)",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download a batch described by JSON Lines (one job per line).
    Run(RunArgs),
    /// Download a single URL.
    Get(GetArgs),
}

/// 两种入口共用的网络与下载选项。
#[derive(Args, Clone)]
struct CommonArgs {
    /// Parallel connections per file (1-32, default 8).
    #[arg(long)]
    connections: Option<usize>,
    /// Attempts per request including the first (default 3).
    #[arg(long)]
    retry_attempts: Option<usize>,
    /// ffmpeg executable (default: resources/ffmpeg next to the binary, then PATH).
    #[arg(long)]
    ffmpeg: Option<String>,
    /// Proxy URL (http:// or socks5://). Default: system proxy.
    #[arg(long)]
    proxy: Option<String>,
    /// Ignore any proxy configuration.
    #[arg(long)]
    no_proxy: bool,
    /// Bind outgoing connections to this network interface (loopback stays local).
    #[arg(long)]
    interface: Option<String>,
    /// DNS-over-HTTPS endpoint instead of the system resolver, e.g. https://dns.example.com/resolve
    #[arg(long)]
    dns_doh: Option<String>,
    /// Fixed IPv4 of the DNS-over-HTTPS endpoint (skips system resolution).
    #[arg(long)]
    dns_doh_ip: Option<String>,
    /// Extra request header `Name: value` (repeatable); a job's own headers win.
    #[arg(long = "header")]
    headers: Vec<String>,
    /// Skip ffmpeg verification (size check only).
    #[arg(long)]
    no_verify: bool,
    /// Do not compute MD5.
    #[arg(long)]
    no_checksum: bool,
    /// Minimum plausible output size in bytes (default 16384).
    #[arg(long)]
    min_output_bytes: Option<u64>,
}

impl CommonArgs {
    fn config(&self) -> Result<Config, String> {
        let mut config = Config::default();
        if let Some(connections) = self.connections {
            config.connections = connections;
        }
        if let Some(attempts) = self.retry_attempts {
            config.retry_attempts = attempts;
        }
        config.ffmpeg = self.ffmpeg.clone();
        config.proxy = self.proxy.clone();
        config.no_proxy = self.no_proxy;
        config.interface = self.interface.clone();
        config.headers = parse_headers(&self.headers)?;
        config.verify = !self.no_verify;
        config.checksum = !self.no_checksum;
        if let Some(minimum) = self.min_output_bytes {
            config.min_output_bytes = minimum;
        }
        match (self.dns_doh.as_deref(), self.dns_doh_ip.as_deref()) {
            (Some(endpoint), Some(ip)) => {
                let host = url::Url::parse(endpoint)
                    .map_err(|_| "dns-doh 不是合法 URL".to_string())?
                    .host_str()
                    .ok_or_else(|| "dns-doh 缺少主机名".to_string())?
                    .to_string();
                let ip = ip
                    .parse::<std::net::Ipv4Addr>()
                    .map_err(|_| "dns-doh-ip 不是 IPv4 地址".to_string())?;
                config.dns = Some(std::sync::Arc::new(DohJsonResolver::new(
                    endpoint.to_string(),
                    host,
                    ip,
                    config.interface.clone(),
                )));
            }
            (None, None) => {}
            _ => return Err("dns-doh 与 dns-doh-ip 必须同时提供".into()),
        }
        Ok(config)
    }
}

#[derive(Args)]
struct RunArgs {
    /// JSON Lines task file, or `-` for stdin.
    #[arg(long)]
    tasks: String,
    /// Also write flat result lines (one Outcome per line) to this file.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Emit throttled progress events on stdout.
    #[arg(long)]
    progress: bool,
    /// Concurrent jobs (1-32, default 3).
    #[arg(long)]
    concurrency: Option<usize>,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(Args)]
struct GetArgs {
    /// Direct URL (HLS playlists are detected automatically).
    url: String,
    /// Output file path.
    #[arg(short, long)]
    out: PathBuf,
    /// Audio stream URL: present means DASH (download both, then merge).
    #[arg(long)]
    audio: Option<String>,
    /// Fallback URL (repeatable).
    #[arg(long)]
    fallback: Vec<String>,
    /// Page URL used to infer the Referer for unknown hosts.
    #[arg(long)]
    page_url: Option<String>,
    /// Print the Outcome as a single JSON line instead of a human summary.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    common: CommonArgs,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ERROR - 无法启动运行时: {error}");
            return ExitCode::from(3);
        }
    };
    let code = match cli.command {
        Command::Run(args) => runtime.block_on(run(args)),
        Command::Get(args) => runtime.block_on(get(args)),
    };
    ExitCode::from(code)
}

fn build_client(config: Config) -> Result<rvdown::Client, u8> {
    match rvdown::Client::builder().config(config).build() {
        Ok(client) => Ok(client),
        Err(error) => {
            eprintln!("ERROR - {error}");
            Err(3)
        }
    }
}

/// `run`：批量 JSONL。
async fn run(args: RunArgs) -> u8 {
    let mut config = match args.common.config() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("ERROR - {message}");
            return 2;
        }
    };
    if let Some(concurrency) = args.concurrency {
        config.job_concurrency = concurrency;
    }
    // 校验顺序：任务文件/结果文件属于「参数错误」（2），必须先于环境初始化（3），
    // 否则 `--tasks 不存在 --proxy bad` 会报客户端错误，与文档的退出码语义不符。
    let jobs = match read_jobs(&args.tasks) {
        Ok(jobs) => jobs,
        Err(message) => {
            eprintln!("ERROR - {message}");
            return 2;
        }
    };
    let client = match build_client(config) {
        Ok(client) => client,
        Err(code) => return code,
    };
    let out_file = match args.out.as_deref() {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => Some(Mutex::new(file)),
            Err(error) => {
                eprintln!("ERROR - 无法写结果文件 {}: {error}", path.display());
                return 2;
            }
        },
        None => None,
    };

    let emitter = Emitter {
        progress: args.progress.then(ProgressThrottle::default),
        out_file,
    };
    let sink: &Emitter = &emitter;
    let concurrency = client.config().job_concurrency;
    let total = jobs.len();
    let mut ok = 0usize;
    let mut failed = 0usize;
    emitter.event(&serde_json::json!({"event": "start", "total": total}));

    // 已完成/失败的索引；中断时把剩下的按 CANCELLED 补齐（保 `.part` 可续传）。
    let mut finished = vec![false; total];
    let mut interrupted = false;

    {
        let mut work = stream::iter(jobs.iter().enumerate().map(|(index, job)| {
            let client = &client;
            async move { (index, client.run_with_progress(job, Some(sink)).await) }
        }))
        .buffer_unordered(concurrency);
        let ctrl_c = tokio::signal::ctrl_c();
        tokio::pin!(ctrl_c);
        loop {
            tokio::select! {
                next = work.next() => match next {
                    Some((index, outcome)) => {
                        finished[index] = true;
                        if outcome.ok { ok += 1 } else { failed += 1 }
                        emitter.result(&outcome);
                    }
                    None => break,
                },
                signal = &mut ctrl_c, if !interrupted => {
                    if signal.is_ok() {
                        interrupted = true;
                        break;
                    }
                }
            }
        }
    }

    if interrupted {
        for (index, job) in jobs.iter().enumerate() {
            if finished[index] {
                continue;
            }
            failed += 1;
            emitter.result(&Outcome {
                id: job.id.clone(),
                ok: false,
                code: ErrorCode::Cancelled,
                out: None,
                bytes: None,
                md5: None,
                duration: None,
                has_video: None,
                has_audio: None,
                source_url: None,
                http_status: None,
                retryable: ErrorCode::Cancelled.is_retryable(),
                message: ErrorCode::Cancelled.description().to_string(),
                detail: None,
                attempts: 0,
                network_bytes: 0,
                resumed_bytes: 0,
                elapsed_ms: 0,
                warnings: Vec::new(),
            });
        }
    }

    emitter.event(&serde_json::json!({
        "event": "summary",
        "total": total,
        "ok": ok,
        "failed": failed,
        "interrupted": interrupted,
    }));
    if interrupted {
        return 130;
    }
    if failed > 0 {
        1
    } else {
        0
    }
}

/// `get`：单文件。
async fn get(args: GetArgs) -> u8 {
    let config = match args.common.config() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("ERROR - {message}");
            return 2;
        }
    };
    let client = match build_client(config) {
        Ok(client) => client,
        Err(code) => return code,
    };
    let job = Job {
        id: args
            .out
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        url: args.url,
        urls: args.fallback,
        audio_url: args.audio,
        out: args.out,
        headers: parse_headers(&args.common.headers).unwrap_or_default(),
        page_url: args.page_url,
        verify: None,
        checksum: None,
    };
    let bar = ProgressLine::new(std::io::stderr().is_terminal());
    let outcome = client.run_with_progress(&job, Some(&bar)).await;
    bar.finish();
    if args.json {
        println!("{}", serde_json::to_string(&outcome).unwrap_or_default());
    } else if outcome.ok {
        println!(
            "OK {} ({} bytes{})",
            outcome.out.as_deref().unwrap_or_default(),
            outcome.bytes.unwrap_or(0),
            outcome
                .md5
                .as_deref()
                .map(|md5| format!(", md5 {md5}"))
                .unwrap_or_default()
        );
        for warning in &outcome.warnings {
            eprintln!("WARN - {}: {}", warning.code, warning.message);
        }
    } else {
        eprintln!(
            "ERROR - {} ({}){}",
            outcome.message,
            outcome.code.description(),
            outcome
                .detail
                .as_deref()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        );
    }
    if outcome.ok {
        0
    } else {
        1
    }
}

/// 读任务：JSON Lines（每行一个对象）；空行跳过，坏行直接报错（不猜、不跳过）。
fn read_jobs(path: &str) -> Result<Vec<Job>, String> {
    let reader: Box<dyn BufRead> = if path == "-" {
        Box::new(std::io::BufReader::new(std::io::stdin()))
    } else {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("无法读取任务文件 {path}: {error}"))?;
        Box::new(std::io::BufReader::new(file))
    };
    let mut jobs = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(|error| format!("读取任务文件失败: {error}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let job = serde_json::from_str::<Job>(trimmed)
            .map_err(|error| format!("第 {} 行不是合法任务: {error}", index + 1))?;
        jobs.push(job);
    }
    if jobs.is_empty() {
        return Err("任务文件里没有任何任务".into());
    }
    Ok(jobs)
}

fn parse_headers(pairs: &[String]) -> Result<HashMap<String, String>, String> {
    let mut headers = HashMap::new();
    for pair in pairs {
        let Some((name, value)) = pair.split_once(':') else {
            return Err(format!("请求头格式应为 `名:值`，收到: {pair}"));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err("请求头名不能为空".into());
        }
        headers.insert(name.to_string(), value.trim().to_string());
    }
    Ok(headers)
}

/// stdout 事件与 `--out` 结果行的统一出口。
struct Emitter {
    progress: Option<ProgressThrottle>,
    out_file: Option<Mutex<std::fs::File>>,
}

impl Emitter {
    fn event(&self, value: &serde_json::Value) {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{value}");
        let _ = stdout.flush();
    }

    fn result(&self, outcome: &Outcome) {
        let mut line = serde_json::to_value(outcome).unwrap_or_default();
        if let Some(object) = line.as_object_mut() {
            object.insert("event".into(), "result".into());
        }
        self.event(&line);
        if let Some(file) = self.out_file.as_ref() {
            if let Ok(mut file) = file.lock() {
                if let Ok(text) = serde_json::to_string(outcome) {
                    let _ = writeln!(file, "{text}");
                    let _ = file.flush();
                }
            }
        }
    }
}

impl ProgressSink for Emitter {
    fn on_item(&self, message: &str) {
        // 人类可读日志只进 stderr：stdout 是调用方解析的协议流。
        eprintln!("INFO - {message}");
    }

    fn on_event(&self, event: &TransferEvent) {
        let Some(throttle) = self.progress.as_ref() else {
            return;
        };
        if matches!(event.phase, TransferPhase::Done | TransferPhase::Failed)
            || !throttle.should_emit(event)
        {
            return;
        }
        self.event(&serde_json::json!({
            "event": "progress",
            "id": event.task_id,
            "asset": event.asset_id,
            "attempt": event.attempt_id,
            "phase": event.phase.code(),
            "validated_bytes": event.validated_bytes,
            "resumed_bytes": event.resumed_bytes,
            "network_bytes": event.network_bytes,
            "total_bytes": event.total_bytes,
            "percent": event.percent(),
        }));
    }
}

/// `run --progress` 的节流：同一任务同一资产在窗口内最多一条，避免把协议流刷爆。
#[derive(Default)]
struct ProgressThrottle {
    last: Mutex<HashMap<(String, String), Instant>>,
}

impl ProgressThrottle {
    fn should_emit(&self, event: &TransferEvent) -> bool {
        let Ok(mut last) = self.last.lock() else {
            return false;
        };
        let key = (event.task_id.clone(), event.asset_id.clone());
        match last.get(&key) {
            Some(previous) if previous.elapsed() < PROGRESS_INTERVAL => false,
            _ => {
                last.insert(key, Instant::now());
                true
            }
        }
    }
}

/// stderr 单行进度（`get` 用）：TTY 下用 `\r` 覆盖，管道退化为逐行。
struct ProgressLine {
    interactive: AtomicU8,
    last: Mutex<Instant>,
}

impl ProgressLine {
    fn new(interactive: bool) -> Self {
        Self {
            interactive: AtomicU8::new(u8::from(interactive)),
            last: Mutex::new(Instant::now()),
        }
    }

    fn finish(&self) {
        if self.interactive.load(Ordering::Relaxed) == 1 {
            eprintln!();
        }
    }
}

impl ProgressSink for ProgressLine {
    fn on_event(&self, event: &TransferEvent) {
        if matches!(event.phase, TransferPhase::Done | TransferPhase::Failed) {
            return;
        }
        let Ok(mut last) = self.last.lock() else {
            return;
        };
        if last.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        *last = Instant::now();
        let text = format!(
            "{}: {}/{} ({})",
            event.phase.label(),
            rvdown::format_bytes(event.validated_bytes),
            event
                .total_bytes
                .map(rvdown::format_bytes)
                .unwrap_or_else(|| "—".to_string()),
            event
                .percent()
                .map(|percent| format!("{percent}%"))
                .unwrap_or_else(|| "—".to_string())
        );
        if self.interactive.load(Ordering::Relaxed) == 1 {
            eprint!("\r{text}\x1b[K");
        } else {
            eprintln!("{text}");
        }
    }
}
