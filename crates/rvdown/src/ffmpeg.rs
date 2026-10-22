//! 外置 ffmpeg 适配：合并（DASH 音视频）、HLS 分片封装、产物检验、封面截帧。
//!
//! 合并用 `-map 0:v:0 -map 1:a:0 -c copy -movflags +faststart`（流拷贝，不重编码）；
//! 检验用 `ffmpeg -i` 只读文件头（没指定输出文件时退出码恒为 1，所以**只看 stderr
//! 内容不看退出码**）；HLS 用 concat demuxer 封装；封面在第 3 秒截帧兜底。
//!
//! **ffmpeg 只外置**（同目录 `resources/ffmpeg[.exe]`，或显式路径，或 `PATH`），不内嵌、不引纯 Rust remux；
//! `CommandRunner` 是唯一接缝——测试注入假实现，禁网禁 ffmpeg 也能把编排测全。
//!
//! 进程等待口径：**不在 poll 线程上阻塞**——子进程在工作线程里 `try_wait` 轮询，
//! 超时就 `kill` + `wait` 回收（绝不留僵尸），结果走 `futures::channel::oneshot`
//! 交回 async 侧（runtime 无关，pollster 也能跑）。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;

/// 缺少 ffmpeg 的固定话术（用户可见文案固定）。
pub const MISSING_FFMPEG_MESSAGE: &str = "缺少 ffmpeg，无法合并音视频";

/// 合并音视频的默认超时：本地流拷贝，正常几秒内完成。
pub const DEFAULT_MUX_TIMEOUT: Duration = Duration::from_secs(120);
/// 读文件头的默认超时。
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// 截帧默认超时。
pub const DEFAULT_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// 低于这个大小基本不可能是正常成片（错误页 / 空文件 / 截断）。
pub const MIN_PLAUSIBLE_VIDEO_BYTES: u64 = 16 * 1024;
/// 单个输出流最多保留这么多字节（防 ffmpeg 把日志打爆内存）。
pub const MAX_CAPTURE_BYTES: usize = 256 * 1024;
/// 报错文案里只取这么多字符做摘要。
pub const SUMMARY_CHARS: usize = 400;

/// 外部进程执行结果。
#[derive(Debug, Clone, Default)]
pub struct ProcessOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    /// **完整** stderr（限长）：`ffmpeg -i` 的流信息（Duration/Stream）在中段，
    /// 截尾会把要解析的字段截掉。报错文案请另用 `stderr_summary`。
    pub stderr: String,
}

/// 报错文案用的日志摘要（与解析输入分开，避免「解析要全文、日志要短」互相迁就）。
pub fn stderr_summary(stderr: &str) -> String {
    stderr.trim().chars().take(SUMMARY_CHARS).collect()
}

/// 外部进程执行接缝：换实现只改这里，不动 ffmpeg 编排。
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// 跑外部进程；`timeout` 到点必须杀掉并回收，绝不让子进程悬挂。
    async fn run(
        &self,
        program: &Path,
        args: &[String],
        timeout: Duration,
    ) -> Result<ProcessOutput, FfmpegError>;
}

/// 默认实现：工作线程 `try_wait` 轮询 + oneshot 交回结果（**不阻塞 poll 线程**）。
#[derive(Debug, Default, Clone, Copy)]
pub struct StdCommandRunner;

#[async_trait]
impl CommandRunner for StdCommandRunner {
    async fn run(
        &self,
        program: &Path,
        args: &[String],
        timeout: Duration,
    ) -> Result<ProcessOutput, FfmpegError> {
        let program = program.to_path_buf();
        let args = args.to_vec();
        let (tx, rx) = futures::channel::oneshot::channel();
        // 子进程的等待/超时/回收全部在工作线程里做：async 侧只 await oneshot，
        // 让出执行权，不会把 poll 线程卡死（对照 `不阻塞poll线程` 测试）。
        std::thread::spawn(move || {
            let _ = tx.send(run_blocking(&program, &args, timeout));
        });
        rx.await
            .map_err(|_| FfmpegError::Failed("ffmpeg 进程等待失败".into()))?
    }
}

/// 工作线程体：spawn → 限内存读流 → `try_wait` 轮询到退出或超时 → 必要时 kill+回收。
fn run_blocking(
    program: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<ProcessOutput, FfmpegError> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                FfmpegError::MissingBinary
            } else {
                FfmpegError::Failed(error.to_string())
            }
        })?;

    // 流读取放独立线程且**限内存**：ffmpeg 可能打很多日志，不能全堆在内存里。
    let stdout = child
        .stdout
        .take()
        .map(|stream| std::thread::spawn(move || read_bounded(stream, MAX_CAPTURE_BYTES)));
    let stderr = child
        .stderr
        .take()
        .map(|stream| std::thread::spawn(move || read_bounded_text(stream, MAX_CAPTURE_BYTES)));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // 超时：先杀再回收，绝不留僵尸进程。
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(FfmpegError::Timeout(timeout));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(FfmpegError::Failed(error.to_string()));
            }
        }
    };

    let stdout = stdout
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    let stderr = stderr
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(ProcessOutput {
        status: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

/// 限内存读完一条流（超限后继续丢弃，避免子进程因管道写满而卡住）。
fn read_bounded(mut stream: impl std::io::Read, max: usize) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buffer.len() < max {
                    let take = n.min(max - buffer.len());
                    buffer.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
    buffer
}

fn read_bounded_text(stream: impl std::io::Read, max: usize) -> String {
    String::from_utf8_lossy(&read_bounded(stream, max)).into_owned()
}

/// ffmpeg 侧的失败分类。
#[derive(Debug, thiserror::Error)]
pub enum FfmpegError {
    #[error("{MISSING_FFMPEG_MESSAGE}")]
    MissingBinary,
    #[error("ffmpeg 执行超时（{0:?}），已终止")]
    Timeout(Duration),
    #[error("无法合并音视频: {0}")]
    Mux(String),
    #[error("无法封装播放列表: {0}")]
    Concat(String),
    #[error("无法检验下载产物: {0}")]
    Probe(String),
    #[error("无法截取封面: {0}")]
    Frame(String),
    #[error("ffmpeg 执行失败: {0}")]
    Failed(String),
}

/// 外置 ffmpeg 的调用面：合并 / 封装 / 检验 / 截帧。
pub struct Ffmpeg {
    program: PathBuf,
    runner: std::sync::Arc<dyn CommandRunner>,
}

impl Ffmpeg {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self::with_runner(program.into(), std::sync::Arc::new(StdCommandRunner))
    }

    pub fn with_runner(program: PathBuf, runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        Self { program, runner }
    }

    /// 默认路径：显式配置 → 程序目录下 resources → PATH，与启动 cwd 无关。
    pub fn resolve_program_in(directory: &Path, configured: Option<&str>) -> PathBuf {
        if let Some(path) = configured.map(str::trim).filter(|path| !path.is_empty()) {
            return crate::util::parse_path_input(path);
        }
        let bundled = directory.join("resources").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if bundled.is_file() {
            return bundled;
        }
        PathBuf::from("ffmpeg")
    }

    /// 未显式配置时的默认路径：本程序目录下 resources → PATH。
    /// **不读环境变量**（与 `Config` 一致）：`FFMPEG_PATH` 之类是调用方自己的约定，
    /// 需要时由调用方读出后通过 `Config::ffmpeg` / `--ffmpeg` 传入。
    pub fn resolve_program() -> PathBuf {
        let directory = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        Self::resolve_program_in(directory.as_deref().unwrap_or(Path::new(".")), None)
    }

    /// `ffmpeg -version` 成功且输出像 ffmpeg 才算可用（组件下发后自检用）。
    pub async fn version(&self) -> Result<String, String> {
        let output = self
            .runner
            .run(&self.program, &["-version".into()], Duration::from_secs(5))
            .await
            .map_err(|error| error.to_string())?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status != 0 || !stdout.starts_with("ffmpeg version") {
            return Err("ffmpeg 自检失败".into());
        }
        Ok(stdout.lines().next().unwrap_or_default().to_string())
    }

    /// HLS 分片拼接：`-f concat -safe 0 -c copy`（concat demuxer 口径）。
    pub async fn concat(&self, list: &Path, dest: &Path) -> Result<(), FfmpegError> {
        let mut args: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        args.push(list.to_string_lossy().into_owned());
        args.extend(["-c", "copy", "-y"].into_iter().map(str::to_string));
        args.push(dest.to_string_lossy().into_owned());
        let output = self
            .runner
            .run(&self.program, &args, DEFAULT_MUX_TIMEOUT)
            .await
            .map_err(wrap_concat)?;
        if output.status != 0 {
            return Err(FfmpegError::Concat(stderr_summary(&output.stderr)));
        }
        Ok(())
    }

    /// DASH 双流合并：流拷贝不重编码（流拷贝参数矩阵）。
    pub async fn mux(&self, video: &Path, audio: &Path, dest: &Path) -> Result<(), FfmpegError> {
        let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-i"]
            .into_iter()
            .map(str::to_string)
            .collect();
        args.push(video.to_string_lossy().into_owned());
        args.push("-i".into());
        args.push(audio.to_string_lossy().into_owned());
        args.extend(
            [
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c",
                "copy",
                "-movflags",
                "+faststart",
                "-y",
            ]
            .into_iter()
            .map(str::to_string),
        );
        args.push(dest.to_string_lossy().into_owned());
        let output = self
            .runner
            .run(&self.program, &args, DEFAULT_MUX_TIMEOUT)
            .await
            .map_err(wrap_mux)?;
        if output.status != 0 {
            return Err(FfmpegError::Mux(stderr_summary(&output.stderr)));
        }
        Ok(())
    }

    /// 产物基础检验：`ffmpeg -i` 只读文件头。**只看 stderr 内容，不看退出码**
    /// （没指定输出文件时 ffmpeg 恒退出 1，只看 stderr）。
    ///
    /// 解析喂**完整** stderr：Duration/Stream 常在中段，截尾会把要解析的字段截掉。
    pub async fn probe(&self, path: &Path) -> Result<ProbeReading, FfmpegError> {
        let args: Vec<String> = ["-hide_banner", "-i"]
            .into_iter()
            .map(str::to_string)
            .chain([path.to_string_lossy().into_owned()])
            .collect();
        let output = self
            .runner
            .run(&self.program, &args, DEFAULT_PROBE_TIMEOUT)
            .await
            .map_err(wrap_probe)?;
        let _ = output.status;
        Ok(parse_probe_output(&output.stderr))
    }

    /// 封面截帧兜底：第 `at_seconds` 秒截一帧（默认 3 秒）。
    pub async fn grab_frame(
        &self,
        video: &Path,
        dest: &Path,
        at_seconds: f64,
    ) -> Result<(), FfmpegError> {
        let args: Vec<String> = vec![
            "-hide_banner".to_string(),
            "-loglevel".to_string(),
            "error".to_string(),
            "-ss".to_string(),
            format!("{at_seconds:.3}"),
            "-i".to_string(),
            video.to_string_lossy().into_owned(),
            "-frames:v".to_string(),
            "1".to_string(),
            "-y".to_string(),
            dest.to_string_lossy().into_owned(),
        ];
        let output = self
            .runner
            .run(&self.program, &args, DEFAULT_FRAME_TIMEOUT)
            .await
            .map_err(wrap_frame)?;
        if output.status != 0 {
            return Err(FfmpegError::Frame(stderr_summary(&output.stderr)));
        }
        Ok(())
    }
}

fn wrap_concat(error: FfmpegError) -> FfmpegError {
    match error {
        FfmpegError::MissingBinary => FfmpegError::MissingBinary,
        FfmpegError::Timeout(value) => FfmpegError::Timeout(value),
        other => FfmpegError::Concat(other.to_string()),
    }
}

fn wrap_mux(error: FfmpegError) -> FfmpegError {
    match error {
        FfmpegError::MissingBinary => FfmpegError::MissingBinary,
        FfmpegError::Timeout(value) => FfmpegError::Timeout(value),
        other => FfmpegError::Mux(other.to_string()),
    }
}

fn wrap_probe(error: FfmpegError) -> FfmpegError {
    match error {
        FfmpegError::MissingBinary => FfmpegError::MissingBinary,
        FfmpegError::Timeout(value) => FfmpegError::Timeout(value),
        other => FfmpegError::Probe(other.to_string()),
    }
}

fn wrap_frame(error: FfmpegError) -> FfmpegError {
    match error {
        FfmpegError::MissingBinary => FfmpegError::MissingBinary,
        FfmpegError::Timeout(value) => FfmpegError::Timeout(value),
        other => FfmpegError::Frame(other.to_string()),
    }
}

/// 从 ffmpeg stderr 里读出的流信息。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeReading {
    pub has_video: bool,
    pub has_audio: bool,
    pub duration_seconds: Option<f64>,
    /// 完全读不出流信息时的 stderr 摘要。
    pub detail: Option<String>,
}

/// 解析 `ffmpeg -i` 的 stderr：看到 `Stream #..: Video:` / `Audio:` 即认定存在。
pub fn parse_probe_output(stderr: &str) -> ProbeReading {
    let has_video = stderr.contains("Video:");
    let has_audio = stderr.contains("Audio:");
    let duration_seconds = parse_duration(stderr);
    let has_streams = stderr.contains("Stream #");
    ProbeReading {
        has_video,
        has_audio,
        duration_seconds,
        detail: if has_streams {
            None
        } else {
            Some(stderr_summary(stderr))
        },
    }
}

fn parse_duration(stderr: &str) -> Option<f64> {
    let start = stderr.find("Duration:")?;
    let rest = &stderr[start + "Duration:".len()..];
    let token = rest.split_whitespace().next()?;
    let token = token.trim_end_matches(',');
    let mut parts = token.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    let total = hours * 3600.0 + minutes * 60.0 + seconds;
    if total.is_finite() && total >= 0.0 {
        Some(total)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct FakeRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        output: ProcessOutput,
    }

    impl FakeRunner {
        fn with_status(status: i32, stderr: &str) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                output: ProcessOutput {
                    status,
                    stdout: Vec::new(),
                    stderr: stderr.to_string(),
                },
            })
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            program: &Path,
            args: &[String],
            _timeout: Duration,
        ) -> Result<ProcessOutput, FfmpegError> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string_lossy().into_owned(), args.to_vec()));
            Ok(self.output.clone())
        }
    }

    fn args_of(runner: &Arc<FakeRunner>) -> Vec<String> {
        runner.calls.lock().unwrap()[0].1.clone()
    }

    #[tokio::test]
    async fn 合并是流拷贝不重编码且不覆盖入参() {
        let runner = FakeRunner::with_status(0, "");
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        ffmpeg
            .mux(Path::new("v.mp4"), Path::new("a.m4a"), Path::new("out.mp4"))
            .await
            .unwrap();
        let expected: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            "v.mp4",
            "-i",
            "a.m4a",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            "-y",
            "out.mp4",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(args_of(&runner), expected, "参数矩阵必须流拷贝不重编码");
    }

    #[tokio::test]
    async fn hls分片用concat封装不把播放列表当成品() {
        let runner = FakeRunner::with_status(0, "");
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        ffmpeg
            .concat(Path::new("list.txt"), Path::new("out.mp4"))
            .await
            .unwrap();
        let args = args_of(&runner);
        assert!(
            args.contains(&"-f".to_string()) && args.contains(&"concat".to_string()),
            "必须走 concat demuxer: {args:?}"
        );
        assert!(args.contains(&"-safe".to_string()));
        assert!(args.contains(&"-c".to_string()) && args.contains(&"copy".to_string()));
    }

    #[tokio::test]
    async fn 产物检验只看stderr不看退出码() {
        // 没指定输出文件时 ffmpeg 恒退出 1：只看 stderr。
        let runner = FakeRunner::with_status(
            1,
            "  Duration: 00:00:12.50, start: 0.000000, bitrate: 800 kb/s\n\
             Stream #0:0: Video: h264\nStream #0:1: Audio: aac",
        );
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        let reading = ffmpeg.probe(Path::new("v.mp4")).await.unwrap();
        assert!(reading.has_video && reading.has_audio);
        assert_eq!(reading.duration_seconds, Some(12.5));
        assert!(reading.detail.is_none(), "有流信息就不给 stderr 摘要");
    }

    #[tokio::test]
    async fn probe喂完整stderr中段的流信息不被截掉() {
        // Duration/Stream 常在中段：前面垫一堆日志行，截尾会把要解析的字段截掉。
        let mut stderr = String::new();
        for index in 0..40 {
            stderr.push_str(&format!("padding log line {index}\n"));
        }
        stderr.push_str("  Duration: 00:00:12.50, start: 0.000000\n");
        stderr.push_str("    Stream #0:0: Video: h264\n");
        stderr.push_str("    Stream #0:1: Audio: aac\n");
        for index in 0..40 {
            stderr.push_str(&format!("trailing log line {index}\n"));
        }
        let runner = FakeRunner::with_status(1, &stderr);
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        let reading = ffmpeg.probe(Path::new("v.mp4")).await.unwrap();
        assert!(
            reading.duration_seconds == Some(12.5) && reading.has_video && reading.has_audio,
            "解析信息与日志摘要分开，中段流信息不能被截掉: {:?}",
            reading
        );
    }

    #[tokio::test]
    async fn 读不出流信息时给stderr摘要() {
        let runner = FakeRunner::with_status(1, "Invalid data found when processing input");
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        let reading = ffmpeg.probe(Path::new("bad.mp4")).await.unwrap();
        assert!(!reading.has_video && !reading.has_audio);
        assert!(reading.detail.unwrap().contains("Invalid data"));
    }

    #[tokio::test]
    async fn 截帧指定第3秒() {
        let runner = FakeRunner::with_status(0, "");
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        ffmpeg
            .grab_frame(Path::new("v.mp4"), Path::new("c.jpg"), 3.0)
            .await
            .unwrap();
        let args = args_of(&runner);
        let ss = args.iter().position(|a| a == "-ss").unwrap();
        assert_eq!(args[ss + 1], "3.000", "默认封面截帧在第 3 秒");
    }

    #[tokio::test]
    async fn 缺少ffmpeg给出固定话术() {
        #[derive(Debug)]
        struct MissingRunner;
        #[async_trait]
        impl CommandRunner for MissingRunner {
            async fn run(
                &self,
                _: &Path,
                _: &[String],
                _: Duration,
            ) -> Result<ProcessOutput, FfmpegError> {
                Err(FfmpegError::MissingBinary)
            }
        }
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), Arc::new(MissingRunner));
        let error = ffmpeg
            .mux(Path::new("v"), Path::new("a"), Path::new("o"))
            .await
            .unwrap_err();
        assert!(matches!(error, FfmpegError::MissingBinary));
        assert_eq!(error.to_string(), MISSING_FFMPEG_MESSAGE);
    }

    #[tokio::test]
    async fn 合并失败不交付并给出排查摘要() {
        let runner = FakeRunner::with_status(1, "Output file is empty");
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        let error = ffmpeg
            .mux(Path::new("v"), Path::new("a"), Path::new("o"))
            .await
            .unwrap_err();
        assert!(matches!(error, FfmpegError::Mux(_)), "{error:?}");
        assert!(error.to_string().contains("Output file is empty"));
    }

    #[test]
    fn 解析时长格式与ts一致() {
        assert_eq!(
            parse_probe_output("Duration: 00:01:02.50").duration_seconds,
            Some(62.5)
        );
        assert_eq!(
            parse_probe_output("Duration: 01:00:00.00").duration_seconds,
            Some(3600.0)
        );
        assert_eq!(
            parse_probe_output("no duration here").duration_seconds,
            None
        );
    }

    #[test]
    fn 日志摘要与解析输入分开() {
        let mut stderr = String::new();
        for index in 0..200 {
            stderr.push_str(&format!("line {index}\n"));
        }
        stderr.push_str("Duration: 00:00:12.50\n");
        let summary = stderr_summary(&stderr);
        assert!(summary.len() <= SUMMARY_CHARS + 8, "摘要要短");
        assert_eq!(
            parse_probe_output(&stderr).duration_seconds,
            Some(12.5),
            "解析喂全文，不喂摘要"
        );
    }

    // ---- 真实子进程（StdCommandRunner）：不阻塞 poll 线程 / 超时杀掉并回收 ----

    /// 可移植的「睡一会儿」命令（unix 用 sh，windows 用 ping）。
    fn sleeper_command(seconds: u64) -> (PathBuf, Vec<String>) {
        if cfg!(windows) {
            (
                PathBuf::from("cmd"),
                vec![
                    "/C".into(),
                    format!("ping -n {} 127.0.0.1 >NUL", seconds + 1),
                ],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec!["-c".into(), format!("sleep {seconds}")],
            )
        }
    }

    #[tokio::test]
    async fn 等待子进程时不阻塞poll线程() {
        // 工作线程里 try_wait 轮询 + oneshot 交回：async 侧必须让出执行权。
        let (program, args) = sleeper_command(1);
        let runner = StdCommandRunner;
        let tick_at = Arc::new(Mutex::new(None::<Instant>));
        let finished_at = Arc::new(Mutex::new(None::<Instant>));
        let tick_record = Arc::clone(&tick_at);
        let finish_record = Arc::clone(&finished_at);

        let run = async move {
            let result = runner.run(&program, &args, Duration::from_secs(10)).await;
            *finish_record.lock().unwrap() = Some(Instant::now());
            result
        };
        // 另一个协程在 50ms 时打点；若 run 阻塞 poll 线程，它永远拿不到执行权。
        let ticker = async move {
            let (tx, rx) = futures::channel::oneshot::channel::<()>();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let _ = tx.send(());
            });
            let _ = rx.await;
            *tick_record.lock().unwrap() = Some(Instant::now());
        };

        let (result, ()) = futures::future::join(run, ticker).await;
        result.unwrap();
        let tick = tick_at.lock().unwrap().take().expect("ticker 要跑完");
        let finish = finished_at.lock().unwrap().take().unwrap();
        assert!(
            tick < finish,
            "等待子进程时必须让出执行权（ticker 打点应在进程结束之前）"
        );
    }

    #[tokio::test]
    async fn 超时杀掉并回收子进程() {
        let (program, args) = sleeper_command(30);
        let runner = StdCommandRunner;
        let started = Instant::now();
        let error = runner
            .run(&program, &args, Duration::from_millis(150))
            .await
            .unwrap_err();
        let elapsed = started.elapsed();
        assert!(matches!(error, FfmpegError::Timeout(_)), "{error:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "超时必须立刻杀掉，不能等满 30 秒: {elapsed:?}"
        );
        // 回收后立刻再跑一次，确认没有悬挂的句柄/僵尸影响后续执行。
        let (fast_program, fast_args) = (PathBuf::from("echo"), vec!["ok".to_string()]);
        let output = runner
            .run(&fast_program, &fast_args, Duration::from_secs(5))
            .await;
        assert!(output.is_ok(), "超时回收后仍可正常执行: {output:?}");
    }

    #[tokio::test]
    async fn 缺失二进制立即报错() {
        let runner = StdCommandRunner;
        let error = runner
            .run(
                &PathBuf::from("definitely-not-a-real-binary-xyz"),
                &[],
                Duration::from_secs(2),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, FfmpegError::MissingBinary), "{error:?}");
    }

    #[test]
    fn 限内存读流不无限膨胀() {
        use std::io::Cursor;
        let big = vec![b'a'; 1024 * 1024];
        let read = read_bounded(Cursor::new(big), 1024);
        assert_eq!(read.len(), 1024, "超限只保留上限字节");
    }

    #[tokio::test]
    async fn 真实进程stderr保留全文供解析() {
        // 用 sh/echo 输出中段带 Duration 的长 stderr，验证不被截尾。
        let (program, args) = if cfg!(windows) {
            (
                PathBuf::from("cmd"),
                vec![
                    "/C".into(),
                    "echo pad 1>&2 && echo Duration: 00:00:12.50 1>&2".into(),
                ],
            )
        } else {
            (
                PathBuf::from("sh"),
                vec![
                    "-c".into(),
                    "for i in $(seq 1 60); do echo pad >&2; done; echo '  Duration: 00:00:12.50' >&2".into(),
                ],
            )
        };
        let runner = StdCommandRunner;
        let output = runner
            .run(&program, &args, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(
            output.stderr.contains("Duration: 00:00:12.50"),
            "stderr 必须保留全文供解析: {:?}",
            output.stderr
        );
    }
}
