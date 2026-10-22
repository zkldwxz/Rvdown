//! Rvdown：通用多线程视频下载内核（Rust SDK）。
//!
//! 只认识 URL：给直链（可带备用候选与音频流）、目标路径、请求头，负责
//! **分段并发下载 → 断点续传 → HLS 封装 / DASH 合并 → 产物校验 → 原子交付**。
//! 不知道任何业务的页面解析或状态存储——那些属于调用方。
//!
//! # 快速开始
//!
//! ```no_run
//! use rvdown::{Config, Job};
//!
//! # async fn run() -> Result<(), rvdown::JobError> {
//! let engine = rvdown::Engine::new(Config::default())?;
//! let job = Job {
//!     id: "demo".into(),
//!     url: "https://cdn.example.com/video.mp4".into(),
//!     out: "/tmp/video.mp4".into(),
//!     ..Default::default()
//! };
//! let outcome = engine.run(&job).await;
//! assert!(outcome.ok);
//! # Ok(())
//! # }
//! ```
//!
//! # 分层
//!
//! - [`Engine`]：高层任务执行器（候选回退、DASH 合并、校验），SDK 与可执行文件共用。
//! - [`FileDownloader`] / [`MediaDownloader`]：中层纯文件落盘，自带 HLS 识别。
//! - [`transport::segment::RangeDownloader`]：底层 Range 分段编排，可注入自定义 HTTP 实现。
//!
//! 设计不变量：
//! - **绝不交付坏产物**：`.part` 收齐、大小核对、原子 rename 才落目标名。
//! - **资源变化不拼片**：验证器/长度/重定向目标变化时从零重下，不把两份内容拼在一起。
//! - **瞬时故障才重试**：磁盘/权限/路径类永久错立即失败。
//! - **DASH 缺音频硬失败**：绝不交付没有声音的成片。

pub mod config;
pub mod error;
pub mod ffmpeg;
mod ffmpeg_adapter;
pub mod headers;
pub mod hls;
pub mod job;
pub mod media;
pub mod net;
pub mod traits;
pub mod transfer;
pub mod transport;
pub mod types;
pub mod util;

mod client;
mod engine;

pub use client::Client;
pub use config::Config;
pub use engine::{download_urls, ffmpeg_version, Engine, JobError, Outcome, Warning};
pub use error::{DownloadError, DownloadFailureKind, ErrorCode, MuxError, ProbeError};
pub use ffmpeg::{CommandRunner, Ffmpeg, FfmpegError, ProbeReading, StdCommandRunner};
pub use headers::{
    default_download_headers, merge_download_headers, resolve_request_headers, HeaderSources,
    HeaderTarget, DEFAULT_DOWNLOAD_USER_AGENT,
};
pub use hls::{
    highest_bandwidth_variant, is_hls_content, parse_playlist, HlsError, HlsFailure,
    HlsMaterializer, Playlist, Variant,
};
pub use job::Job;
pub use media::MediaDownloader;
pub use net::{DnsResolver, DohJsonResolver};
pub use traits::{
    AudioMuxer, DownloadContext, FileDownloader, FrameGrabber, ProgressSink, VideoProbe,
};
pub use transfer::progress::{
    format_bytes, format_rate, RateMeter, TransferEvent, TransferPhase, TransferReport,
    TransferTrack,
};
pub use transfer::watchdog::LowSpeedPolicy;
pub use transport::segment::{RangeDownloader, RangeDownloaderConfig, TransferPolicy};
pub use transport::wreq_client::{TransportConfig, WreqSegmentDownloader, WREQ_EMULATION};
pub use types::VideoProbeResult;
