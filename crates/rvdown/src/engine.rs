//! 高层任务执行器：候选回退 → DASH 合并 → 产物校验 → 结构化结果。
//!
//! SDK 的 `engine.run(&job)` 与可执行文件的 JSONL 逐行任务走的是**同一段代码**，
//! 所以两种形态的字段、错误码、判定口径天然一致。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{DownloadError, ErrorCode, MuxError, ProbeError};
use crate::ffmpeg::Ffmpeg;
use crate::job::Job;
use crate::media::MediaDownloader;
use crate::traits::{DownloadContext, FileDownloader, ProgressSink};
use crate::transfer::progress::{TransferEvent, TransferPhase, TransferReport};

/// 配置/初始化错误（任务级失败不走这里，全部编码进 `Outcome`）。
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("配置无效: {0}")]
    Config(String),
    #[error("下载客户端初始化失败: {0}")]
    Transport(String),
}

/// 结果里的告警：下载成功但需要调用方知道的事。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Warning {
    /// 稳定标识：`NO_AUDIO_TRACK` / `PROBE_SKIPPED` / `CHECKSUM_SKIPPED`。
    pub code: String,
    pub message: String,
}

impl Warning {
    fn no_audio() -> Self {
        Self {
            code: "NO_AUDIO_TRACK".into(),
            message: "output has no audio track".into(),
        }
    }
    fn probe_skipped(reason: &str) -> Self {
        Self {
            code: "PROBE_SKIPPED".into(),
            message: format!("ffmpeg probe skipped: {reason}"),
        }
    }
}

/// 一条任务的结果（JSONL 输出的一行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub id: String,
    pub ok: bool,
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub out: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    /// 产物容器里的真实时长（秒）；没有 ffmpeg 或读不到时为空。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_video: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_audio: Option<bool>,
    /// 最终成功的那条候选地址。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// 调用方稍后重试同一任务可能成功。
    #[serde(default)]
    pub retryable: bool,
    /// 固定英文短句（由错误码决定，调用方不必解析）。
    pub message: String,
    /// 排查原文（可能含上游文案），不参与任何状态判定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 进度尝试次数（含首次）。
    #[serde(default)]
    pub attempts: u32,
    /// 本次真实网络字节（不含断点恢复）。
    #[serde(default)]
    pub network_bytes: u64,
    #[serde(default)]
    pub resumed_bytes: u64,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

impl Outcome {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            ok: false,
            code: ErrorCode::InvalidJob,
            out: None,
            bytes: None,
            md5: None,
            duration: None,
            has_video: None,
            has_audio: None,
            source_url: None,
            http_status: None,
            retryable: false,
            message: String::new(),
            detail: None,
            attempts: 0,
            network_bytes: 0,
            resumed_bytes: 0,
            elapsed_ms: 0,
            warnings: Vec::new(),
        }
    }

    fn failed(id: &str, code: ErrorCode, detail: impl Into<String>) -> Self {
        let mut outcome = Self::new(id);
        outcome.code = code;
        outcome.message = code.description().to_string();
        outcome.retryable = code.is_retryable();
        let detail = detail.into();
        if !detail.is_empty() {
            outcome.detail = Some(detail);
        }
        outcome
    }

    fn from_download_error(id: &str, error: DownloadError) -> Self {
        let status = match &error {
            DownloadError::Http { status, .. } => Some(*status),
            _ => None,
        };
        let mut outcome = Self::failed(id, error.code(), error.to_string());
        outcome.http_status = status;
        outcome
    }
}

/// 任务执行器。构造一次，重复使用（共享连接池与连接预算）。
pub struct Engine {
    config: Config,
    downloader: MediaDownloader,
    ffmpeg: Arc<Ffmpeg>,
}

impl Engine {
    pub fn new(config: Config) -> Result<Self, JobError> {
        let program = match config.ffmpeg.as_deref() {
            Some(path) => crate::util::parse_path_input(path),
            None => Ffmpeg::resolve_program(),
        };
        Self::with_ffmpeg(config, Ffmpeg::new(program))
    }

    /// 用现成的 ffmpeg 适配构造（可注入自定义 `CommandRunner`，例如沙箱或测试替身）。
    /// 此时 `Config::ffmpeg` 被忽略。
    pub fn with_ffmpeg(config: Config, ffmpeg: Ffmpeg) -> Result<Self, JobError> {
        let config = config.normalized().map_err(JobError::Config)?;
        let ffmpeg = Arc::new(ffmpeg);
        let downloader =
            MediaDownloader::new(ffmpeg.clone(), &config).map_err(|error| match error {
                DownloadError::Network(message) => JobError::Transport(message),
                other => JobError::Transport(other.to_string()),
            })?;
        Ok(Self {
            config,
            downloader,
            ffmpeg,
        })
    }

    /// 生效的配置（已规范化）。
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 底层文件下载器：需要自己编排（例如自行上报进度）的调用方直接用它。
    pub fn downloader(&self) -> &MediaDownloader {
        &self.downloader
    }

    /// 外置 ffmpeg 适配（合并 / 检验 / 截帧），调用方可直接用它做额外处理。
    pub fn ffmpeg(&self) -> &Ffmpeg {
        &self.ffmpeg
    }

    /// 纯文件下载：不做候选回退与媒体校验，只保证字节落盘。
    ///
    /// 适合封面、音频等附属文件；`page_url` 用于按目标域名推断 Referer。
    pub async fn fetch(
        &self,
        url: &str,
        dest: &Path,
        headers: &std::collections::HashMap<String, String>,
        page_url: Option<&str>,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        let context = DownloadContext {
            page_url: page_url.map(str::to_string),
            task_id: None,
            asset_id: None,
        };
        self.downloader
            .download_with_context(url, dest, headers, &context, progress)
            .await
    }

    /// 执行一条任务。**永远返回结果**：所有失败都编码进 `Outcome`。
    pub async fn run(&self, job: &Job) -> Outcome {
        self.run_with_progress(job, None).await
    }

    /// 执行一条任务并回报进度。
    pub async fn run_with_progress(
        &self,
        job: &Job,
        progress: Option<&dyn ProgressSink>,
    ) -> Outcome {
        let started = Instant::now();
        let mut outcome = match self.execute(job, progress).await {
            Ok(outcome) => outcome,
            Err(outcome) => *outcome,
        };
        outcome.elapsed_ms = started.elapsed().as_millis() as u64;
        outcome
    }

    /// 批量执行：最多 `job_concurrency` 条并发，**完成一条就回调一条**（顺序不保证）。
    ///
    /// 进度事件统一走 `progress`；回调里拿到的 `Outcome` 已完整（含告警与耗时）。
    pub async fn run_stream(
        &self,
        jobs: &[Job],
        progress: Option<&dyn ProgressSink>,
        on_outcome: &mut dyn FnMut(Outcome),
    ) {
        let mut completed =
            stream::iter(jobs.iter().map(|job| self.run_with_progress(job, progress)))
                .buffer_unordered(self.config.job_concurrency);
        while let Some(outcome) = completed.next().await {
            on_outcome(outcome);
        }
    }

    /// 批量执行并收集结果（顺序与输入一致）。
    pub async fn run_all(&self, jobs: &[Job]) -> Vec<Outcome> {
        let mut collected: Vec<(usize, Outcome)> = stream::iter(
            jobs.iter()
                .enumerate()
                .map(|(index, job)| async move { (index, self.run(job).await) }),
        )
        .buffer_unordered(self.config.job_concurrency)
        .collect()
        .await;
        collected.sort_by_key(|(index, _)| *index);
        collected.into_iter().map(|(_, outcome)| outcome).collect()
    }

    /// 单条执行：`Err(Outcome)` 表示失败结果。
    async fn execute(
        &self,
        job: &Job,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<Outcome, Box<Outcome>> {
        if let Err((code, detail)) = job.validate() {
            return Err(Box::new(Outcome::failed(&job.id, code, detail)));
        }
        let out = job.out.clone();
        if let Some(parent) = out.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|error| {
                Outcome::from_download_error(
                    &job.id,
                    crate::transport::segment::map_fs_error(error),
                )
            })?;
        }

        // 1. 逐候选下载；永久错（磁盘/权限/路径）立即停，不再试下一个。
        let mut last_error: Option<DownloadError> = None;
        let mut source_url = None;
        let mut report = TransferReport::default();
        for url in job.candidates() {
            match self.transfer(job, url, &out, progress).await {
                Ok(transfer) => {
                    report = transfer;
                    source_url = Some(url.to_string());
                    break;
                }
                Err(error) => {
                    let permanent = error.kind().is_permanent();
                    last_error = Some(error);
                    if permanent {
                        break;
                    }
                }
            }
        }
        let Some(source_url) = source_url else {
            let error = last_error.unwrap_or(DownloadError::Protocol("没有可用的下载地址".into()));
            return Err(Box::new(Outcome::from_download_error(&job.id, error)));
        };

        // 2. 校验与摘要。
        let mut warnings = Vec::new();
        let mut outcome = match self.verify(job, &out, &mut warnings, progress).await {
            Ok(verification) => verification,
            Err(failure) => return Err(failure),
        };

        outcome.id = job.id.clone();
        outcome.ok = true;
        outcome.code = if warnings
            .iter()
            .any(|warning| warning.code == "NO_AUDIO_TRACK")
        {
            ErrorCode::OkNoAudio
        } else {
            ErrorCode::Ok
        };
        outcome.message = outcome.code.description().to_string();
        outcome.out = Some(out.display().to_string());
        outcome.source_url = Some(source_url);
        outcome.attempts = report.attempts;
        outcome.network_bytes = report.network_bytes;
        outcome.resumed_bytes = report.resumed_bytes;
        outcome.warnings = warnings;
        if let Some(sink) = progress {
            sink.on_event(&TransferEvent {
                task_id: job.id.clone(),
                asset_id: "file".into(),
                attempt_id: report.attempts,
                phase: TransferPhase::Done,
                validated_bytes: outcome.bytes.unwrap_or(0),
                resumed_bytes: report.resumed_bytes,
                network_bytes: report.network_bytes,
                total_bytes: outcome.bytes,
            });
        }
        Ok(outcome)
    }

    /// 一条候选地址的下载（含 DASH 合并）。
    async fn transfer(
        &self,
        job: &Job,
        url: &str,
        out: &Path,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        let context = DownloadContext {
            page_url: job.page_url.clone(),
            task_id: Some(job.id.clone()),
            asset_id: Some("video".into()),
        };
        let Some(audio_url) = job.audio_url.as_deref() else {
            return self
                .downloader
                .download_with_context(url, out, &job.headers, &context, progress)
                .await;
        };

        // DASH：两条流都下完才合并；任何一步失败都不交付半成品。
        let video_temp = sibling(out, "videotmp");
        let audio_temp = sibling(out, "audiotmp");
        let video_report = self
            .downloader
            .download_with_context(url, &video_temp, &job.headers, &context, progress)
            .await;
        let video_report = match video_report {
            Ok(report) => report,
            Err(error) => {
                cleanup(&[&video_temp, &audio_temp]);
                return Err(error);
            }
        };
        let audio_context = DownloadContext {
            asset_id: Some("audio".into()),
            ..context.clone()
        };
        let audio_report = self
            .downloader
            .download_with_context(
                audio_url,
                &audio_temp,
                &job.headers,
                &audio_context,
                progress,
            )
            .await;
        let audio_report = match audio_report {
            Ok(report) => report,
            Err(error) => {
                cleanup(&[&video_temp, &audio_temp]);
                return Err(error);
            }
        };
        if let Some(sink) = progress {
            sink.on_event(&TransferEvent {
                task_id: job.id.clone(),
                asset_id: "audio".into(),
                attempt_id: audio_report.attempts,
                phase: TransferPhase::Merging,
                validated_bytes: audio_report.validated_bytes,
                resumed_bytes: audio_report.resumed_bytes,
                network_bytes: audio_report.network_bytes,
                total_bytes: audio_report.total_bytes,
            });
        }
        let muxed =
            crate::ffmpeg_adapter::mux_files(&self.ffmpeg, &video_temp, &audio_temp, out).await;
        cleanup(&[&video_temp, &audio_temp]);
        // 错误码要能让调用方区分「环境缺 ffmpeg」与「合并本身失败」：
        // 前者换机器/装组件可解，后者重下同一素材只会再失败一次。
        muxed.map_err(|error| match error {
            MuxError::MissingBinary => {
                DownloadError::FfmpegMissing(crate::ffmpeg::MISSING_FFMPEG_MESSAGE.into())
            }
            other => DownloadError::MergeFailed(other.to_string()),
        })?;

        let size = std::fs::metadata(out)
            .map_err(crate::transport::segment::map_fs_error)?
            .len();
        if size == 0 {
            cleanup(&[out]);
            return Err(DownloadError::OutputInvalid(
                "合并产物为空，不能交付".into(),
            ));
        }
        Ok(TransferReport {
            network_bytes: video_report.network_bytes + audio_report.network_bytes,
            resumed_bytes: video_report.resumed_bytes + audio_report.resumed_bytes,
            validated_bytes: size,
            attempts: video_report.attempts + audio_report.attempts,
            total_bytes: Some(size),
        })
    }

    /// 产物校验：大小下限 → ffmpeg 流信息 → 可选 MD5。
    async fn verify(
        &self,
        job: &Job,
        out: &Path,
        warnings: &mut Vec<Warning>,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<Outcome, Box<Outcome>> {
        let size = match std::fs::metadata(out) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                return Err(Box::new(Outcome::from_download_error(
                    &job.id,
                    crate::transport::segment::map_fs_error(error),
                )))
            }
        };
        if size < self.config.min_output_bytes {
            cleanup(&[out, &sidecar(out)]);
            return Err(Box::new(Outcome::failed(
                &job.id,
                ErrorCode::OutputTooSmall,
                format!(
                    "产物过小（{size} 字节 < {} 字节），疑似错误页或空文件",
                    self.config.min_output_bytes
                ),
            )));
        }

        let mut outcome = Outcome::new(&job.id);
        outcome.bytes = Some(size);

        if job.verify.unwrap_or(self.config.verify) {
            if let Some(sink) = progress {
                sink.on_event(&TransferEvent {
                    task_id: job.id.clone(),
                    asset_id: "verify".into(),
                    attempt_id: 1,
                    phase: TransferPhase::Verifying,
                    validated_bytes: size,
                    resumed_bytes: 0,
                    network_bytes: 0,
                    total_bytes: Some(size),
                });
            }
            use crate::traits::VideoProbe;
            match <Ffmpeg as VideoProbe>::probe(&self.ffmpeg, out).await {
                Ok(reading) => {
                    outcome.has_video = Some(reading.has_video);
                    outcome.has_audio = Some(reading.has_audio);
                    outcome.duration = reading.duration_seconds;
                    if !reading.has_video {
                        cleanup(&[out, &sidecar(out)]);
                        let detail = reading
                            .detail
                            .as_deref()
                            .filter(|detail| !detail.trim().is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| "ffmpeg 未读到视频轨".to_string());
                        return Err(Box::new(Outcome::failed(
                            &job.id,
                            ErrorCode::OutputNoVideoTrack,
                            detail,
                        )));
                    }
                    if !reading.has_audio {
                        warnings.push(Warning::no_audio());
                    }
                }
                Err(ProbeError::MissingBinary) => {
                    warnings.push(Warning::probe_skipped("ffmpeg executable not found"));
                }
                Err(ProbeError::Failed(_)) => {
                    // 检验本身失败（超时等）：不判死，留给调用方按需复查。
                    warnings.push(Warning::probe_skipped("probe failed"));
                }
            }
        }

        if job.checksum.unwrap_or(self.config.checksum) {
            match crate::util::md5_file(out).await {
                Ok(md5) => outcome.md5 = Some(md5),
                Err(error) => warnings.push(Warning {
                    code: "CHECKSUM_SKIPPED".into(),
                    message: format!("md5 failed: {error}"),
                }),
            }
        }
        Ok(outcome)
    }
}

fn sibling(out: &Path, suffix: &str) -> PathBuf {
    let mut name = out.as_os_str().to_os_string();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

fn sidecar(out: &Path) -> PathBuf {
    let mut name = out.as_os_str().to_os_string();
    name.push(".aria2");
    PathBuf::from(name)
}

fn cleanup(paths: &[&Path]) {
    let mut seen = HashSet::new();
    for path in paths {
        if seen.insert(path.to_path_buf()) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 供 CLI 判断是否有人真正消费了 ffmpeg：先探测一次版本。
pub async fn ffmpeg_version(ffmpeg: &Ffmpeg) -> Result<String, String> {
    ffmpeg.version().await
}

/// 便捷入口：给一组直链直接下载（不经过任务结构体）。
pub async fn download_urls(
    config: Config,
    targets: &[(String, PathBuf)],
) -> Result<Vec<Outcome>, JobError> {
    let engine = Engine::new(config)?;
    let jobs: Vec<Job> = targets
        .iter()
        .map(|(url, out)| Job {
            id: out
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            url: url.clone(),
            out: out.clone(),
            ..Default::default()
        })
        .collect();
    Ok(engine.run_all(&jobs).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome_roundtrip(outcome: &Outcome) -> Outcome {
        serde_json::from_str(&serde_json::to_string(outcome).unwrap()).unwrap()
    }

    #[test]
    fn 结果序列化字段稳定且可往返() {
        let mut outcome = Outcome::failed("a1", ErrorCode::HttpNotFound, "HTTP 404");
        outcome.http_status = Some(404);
        let decoded = outcome_roundtrip(&outcome);
        assert_eq!(decoded.code, ErrorCode::HttpNotFound);
        assert_eq!(decoded.http_status, Some(404));
        assert!(!decoded.ok);
        assert!(!decoded.retryable);

        let mut ok = Outcome::failed("a2", ErrorCode::Ok, "");
        ok.ok = true;
        ok.bytes = Some(1234);
        let decoded = outcome_roundtrip(&ok);
        assert!(decoded.ok);
        assert_eq!(decoded.code, ErrorCode::Ok);
        assert_eq!(decoded.detail, None);
    }

    #[test]
    fn 错误码与http状态对应且可重试判定一致() {
        assert_eq!(ErrorCode::from_http_status(404), ErrorCode::HttpNotFound);
        assert_eq!(ErrorCode::from_http_status(410), ErrorCode::HttpNotFound);
        assert_eq!(ErrorCode::from_http_status(403), ErrorCode::HttpForbidden);
        assert_eq!(ErrorCode::from_http_status(429), ErrorCode::HttpRateLimited);
        assert_eq!(ErrorCode::from_http_status(503), ErrorCode::HttpServerError);
        assert_eq!(ErrorCode::from_http_status(400), ErrorCode::HttpClientError);
        assert!(!ErrorCode::HttpNotFound.is_retryable());
        assert!(ErrorCode::HttpForbidden.is_retryable());
        assert!(!ErrorCode::DiskFull.is_retryable());
    }

    #[test]
    fn 磁盘类永久错误不重试() {
        for error in [
            DownloadError::DiskFull,
            DownloadError::Permission("denied".into()),
            DownloadError::Path("bad".into()),
            DownloadError::Io("io".into()),
        ] {
            assert!(error.kind().is_permanent(), "{error}");
            assert!(!ErrorCode::from(error.kind()).is_retryable());
        }
        assert!(!DownloadError::Timeout.kind().is_permanent());
    }

    // ---- 执行器行为：回环服务器 + 假 ffmpeg（测试注入，不代表真实样本结果） ----

    use crate::ffmpeg::{CommandRunner, FfmpegError, ProcessOutput};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    const VIDEO_AND_AUDIO: &str =
        "Duration: 00:00:10.00\nStream #0:0: Video: h264\nStream #0:1: Audio: aac";
    const VIDEO_ONLY: &str = "Duration: 00:00:10.00\nStream #0:0: Video: h264";
    const AUDIO_ONLY: &str = "Duration: 00:00:10.00\nStream #0:0: Audio: aac";

    /// 路径 → (状态码, 正文)；记录每个路径被请求的次数。
    struct Server {
        base: String,
        hits: Arc<Mutex<HashMap<String, usize>>>,
    }

    impl Server {
        fn start(routes: Vec<(&'static str, u16, Vec<u8>)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let routes: Arc<HashMap<String, (u16, Vec<u8>)>> = Arc::new(
                routes
                    .into_iter()
                    .map(|(path, status, body)| (path.to_string(), (status, body)))
                    .collect(),
            );
            let hits = Arc::new(Mutex::new(HashMap::new()));
            let shared = hits.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let routes = routes.clone();
                    let hits = shared.clone();
                    std::thread::spawn(move || serve(stream, &routes, &hits));
                }
            });
            Self { base, hits }
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }

        fn hits(&self, path: &str) -> usize {
            self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
        }
    }

    fn serve(
        mut stream: std::net::TcpStream,
        routes: &HashMap<String, (u16, Vec<u8>)>,
        hits: &Mutex<HashMap<String, usize>>,
    ) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        if reader.read_line(&mut request).is_err() {
            return;
        }
        let mut range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("range") {
                    range = Some(value.trim().to_string());
                }
            }
        }
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap_or("GET").to_string();
        let path = parts.next().unwrap_or("/").to_string();
        *hits.lock().unwrap().entry(path.clone()).or_default() += 1;
        let Some((status, body)) = routes.get(&path) else {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        };
        if *status != 200 {
            let _ = write!(
                stream,
                "HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            return;
        }
        let total = body.len();
        let (head, payload) = match (method.as_str(), range) {
            ("HEAD", _) => ("200 OK".to_string(), &body[..0]),
            (_, Some(range)) => {
                let spec = range.trim_start_matches("bytes=");
                let (start, end) = spec.split_once('-').unwrap();
                let start: usize = start.parse().unwrap_or(0);
                let end = end
                    .parse::<usize>()
                    .map(|end| end.min(total - 1))
                    .unwrap_or(total - 1);
                let _ = write!(
                    stream,
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nETag: \"v1\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    end - start + 1
                );
                let _ = stream.write_all(&body[start..=end]);
                return;
            }
            _ => ("200 OK".to_string(), &body[..]),
        };
        let length = if method == "HEAD" {
            total
        } else {
            payload.len()
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {head}\r\nContent-Length: {length}\r\nETag: \"v1\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(payload);
    }

    fn media(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// 合并步骤的替身行为。
    #[derive(Clone, Copy)]
    enum Mux {
        /// 往输出写这些字节并成功退出。
        Write(&'static [u8]),
        /// 模拟找不到 ffmpeg。
        Missing,
        /// ffmpeg 非零退出（合并本身失败）。
        Fail,
    }

    struct FakeFfmpeg {
        probe: &'static str,
        mux: Mux,
        mux_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeFfmpeg {
        async fn run(
            &self,
            _: &Path,
            args: &[String],
            _: Duration,
        ) -> Result<ProcessOutput, FfmpegError> {
            if args.iter().any(|arg| arg == "-y") {
                self.mux_calls.fetch_add(1, Ordering::SeqCst);
                match self.mux {
                    Mux::Write(bytes) => std::fs::write(args.last().unwrap(), bytes).unwrap(),
                    Mux::Missing => return Err(FfmpegError::MissingBinary),
                    Mux::Fail => {
                        return Ok(ProcessOutput {
                            status: 1,
                            stdout: vec![],
                            stderr: "Invalid data found when processing input".into(),
                        })
                    }
                }
                return Ok(ProcessOutput {
                    status: 0,
                    stdout: vec![],
                    stderr: String::new(),
                });
            }
            Ok(ProcessOutput {
                status: 1,
                stdout: vec![],
                stderr: self.probe.into(),
            })
        }
    }

    fn engine(probe: &'static str, mux: Mux) -> (Engine, Arc<FakeFfmpeg>) {
        let runner = Arc::new(FakeFfmpeg {
            probe,
            mux,
            mux_calls: AtomicUsize::new(0),
        });
        let config = Config {
            retry_attempts: 1,
            retry_wait: Duration::from_millis(10),
            no_proxy: true,
            ..Config::default()
        };
        let ffmpeg = Ffmpeg::with_runner("ffmpeg".into(), runner.clone());
        (Engine::with_ffmpeg(config, ffmpeg).unwrap(), runner)
    }

    fn job(url: String, out: PathBuf) -> Job {
        Job {
            id: "t".into(),
            url,
            urls: Vec::new(),
            audio_url: None,
            out,
            headers: HashMap::new(),
            page_url: None,
            verify: None,
            checksum: None,
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn 主地址失败按序回退备用地址且记录成功来源() {
        let server = Server::start(vec![("/b.mp4", 200, media(64 * 1024))]);
        let (engine, _) = engine(VIDEO_AND_AUDIO, Mux::Write(b""));
        let dir = tempfile::tempdir().unwrap();
        let mut task = job(server.url("/a.mp4"), dir.path().join("成片.mp4"));
        task.urls = vec![server.url("/b.mp4")];
        let outcome = engine.run(&task).await;
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(outcome.code, ErrorCode::Ok);
        assert_eq!(
            outcome.source_url.as_deref(),
            Some(server.url("/b.mp4").as_str())
        );
        assert_eq!(outcome.bytes, Some(64 * 1024));
        assert!(outcome.md5.is_some());
        assert!(server.hits("/a.mp4") >= 1);
        assert_eq!(entries(dir.path()), vec!["成片.mp4"]);
    }

    #[tokio::test]
    async fn 永久文件系统错误立即停止不再尝试后续候选() {
        let server = Server::start(vec![
            ("/a.mp4", 200, media(64 * 1024)),
            ("/b.mp4", 200, media(64 * 1024)),
        ]);
        let (engine, _) = engine(VIDEO_AND_AUDIO, Mux::Write(b""));
        let dir = tempfile::tempdir().unwrap();
        // 目标路径已是目录：落盘必然失败，属于永久错，换地址也没用。
        let out = dir.path().join("占用.mp4");
        std::fs::create_dir(&out).unwrap();
        let mut task = job(server.url("/a.mp4"), out);
        task.urls = vec![server.url("/b.mp4")];
        let outcome = engine.run(&task).await;
        assert!(!outcome.ok);
        assert!(
            matches!(
                outcome.code,
                ErrorCode::BadPath | ErrorCode::IoError | ErrorCode::PermissionDenied
            ),
            "{outcome:?}"
        );
        assert!(!outcome.retryable);
        assert_eq!(server.hits("/b.mp4"), 0, "永久错不应再试下一个候选");
    }

    #[tokio::test]
    async fn 产物过小判失败并删除残件() {
        let server = Server::start(vec![("/tiny.mp4", 200, media(1024))]);
        let (engine, _) = engine(VIDEO_AND_AUDIO, Mux::Write(b""));
        let dir = tempfile::tempdir().unwrap();
        let outcome = engine
            .run(&job(server.url("/tiny.mp4"), dir.path().join("小.mp4")))
            .await;
        assert_eq!(outcome.code, ErrorCode::OutputTooSmall, "{outcome:?}");
        assert!(!outcome.retryable);
        assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
    }

    #[tokio::test]
    async fn 没有视频轨判失败并删除残件() {
        let server = Server::start(vec![("/v.mp4", 200, media(64 * 1024))]);
        let (engine, _) = engine(AUDIO_ONLY, Mux::Write(b""));
        let dir = tempfile::tempdir().unwrap();
        let outcome = engine
            .run(&job(server.url("/v.mp4"), dir.path().join("无画面.mp4")))
            .await;
        assert_eq!(outcome.code, ErrorCode::OutputNoVideoTrack, "{outcome:?}");
        assert_eq!(outcome.has_video, None);
        assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
    }

    #[tokio::test]
    async fn 单流没有音频轨仍算成功但带告警() {
        let server = Server::start(vec![("/v.mp4", 200, media(64 * 1024))]);
        let (engine, _) = engine(VIDEO_ONLY, Mux::Write(b""));
        let dir = tempfile::tempdir().unwrap();
        let outcome = engine
            .run(&job(server.url("/v.mp4"), dir.path().join("无声.mp4")))
            .await;
        assert!(outcome.ok);
        assert_eq!(outcome.code, ErrorCode::OkNoAudio);
        assert_eq!(outcome.has_audio, Some(false));
        assert!(outcome
            .warnings
            .iter()
            .any(|warning| warning.code == "NO_AUDIO_TRACK"));
    }

    #[tokio::test]
    async fn dash音频失败不合并且清理双流临时件() {
        let server = Server::start(vec![("/v.m4s", 200, media(64 * 1024))]);
        let (engine, runner) = engine(VIDEO_AND_AUDIO, Mux::Write(b"never"));
        let dir = tempfile::tempdir().unwrap();
        let mut task = job(server.url("/v.m4s"), dir.path().join("双流.mp4"));
        task.audio_url = Some(server.url("/a.m4s"));
        let outcome = engine.run(&task).await;
        assert_eq!(outcome.code, ErrorCode::HttpNotFound, "{outcome:?}");
        assert_eq!(runner.mux_calls.load(Ordering::SeqCst), 0, "缺音频绝不合并");
        assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
    }

    #[tokio::test]
    async fn dash缺ffmpeg与合并失败错误码可区分且保留已有成片() {
        let server = Server::start(vec![
            ("/v.m4s", 200, media(64 * 1024)),
            ("/a.m4s", 200, media(32 * 1024)),
        ]);
        for (mux, code) in [
            (Mux::Missing, ErrorCode::FfmpegMissing),
            (Mux::Fail, ErrorCode::MergeFailed),
        ] {
            let (engine, _) = engine(VIDEO_AND_AUDIO, mux);
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("双流.mp4");
            std::fs::write(&out, "原有成片").unwrap();
            let mut task = job(server.url("/v.m4s"), out.clone());
            task.audio_url = Some(server.url("/a.m4s"));
            let outcome = engine.run(&task).await;
            assert_eq!(outcome.code, code, "{outcome:?}");
            assert_eq!(
                std::fs::read_to_string(&out).unwrap(),
                "原有成片",
                "合并失败不能破坏已有成片"
            );
            assert_eq!(entries(dir.path()), vec!["双流.mp4"], "临时件必须清理");
        }
    }

    #[tokio::test]
    async fn dash双流合并成功且网络字节合计() {
        static MERGED: [u8; 40 * 1024] = [7u8; 40 * 1024];
        let server = Server::start(vec![
            ("/v.m4s", 200, media(64 * 1024)),
            ("/a.m4s", 200, media(32 * 1024)),
        ]);
        let (engine, runner) = engine(VIDEO_AND_AUDIO, Mux::Write(&MERGED));
        let dir = tempfile::tempdir().unwrap();
        let mut task = job(server.url("/v.m4s"), dir.path().join("双流.mp4"));
        task.audio_url = Some(server.url("/a.m4s"));
        let outcome = engine.run(&task).await;
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(runner.mux_calls.load(Ordering::SeqCst), 1);
        assert_eq!(outcome.bytes, Some(MERGED.len() as u64));
        assert_eq!(outcome.network_bytes, 96 * 1024);
        assert_eq!(entries(dir.path()), vec!["双流.mp4"]);
    }
}
