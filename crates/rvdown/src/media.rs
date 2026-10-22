//! 默认媒体下载组合：共用浏览器连接池、来源感知 Range、按内容自动识别 HLS。
//!
//! 这是最常用的 `FileDownloader` 实现：调用方给直链、目标路径与请求头，它负责
//! 分段并发、断点续传、HLS 分片落地与封装。DASH 双流合并由上层任务执行器编排。

use crate::{
    config::Config,
    error::DownloadError,
    ffmpeg::Ffmpeg,
    headers::{insert_ci, HeaderSources},
    hls::{normalize_hls_concurrency, HlsFailure, HlsMaterializer},
    traits::{DownloadContext, FileDownloader, ProgressSink},
    transfer::progress::{TransferReport, TransferTrack},
    transport::segment::{map_fs_error, RangeDownloader, RangeDownloaderConfig},
    transport::wreq_client::{TransportConfig, WreqSegmentDownloader},
};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::io::AsyncReadExt;

/// HLS 分片并发：默认按配置，未配置时 16（钳 1..=32）。
fn hls_concurrency(config: &Config) -> usize {
    normalize_hls_concurrency(Some(config.connections.saturating_mul(2)))
}

pub struct MediaDownloader {
    client: WreqSegmentDownloader,
    config: RangeDownloaderConfig,
    policy: crate::transport::segment::TransferPolicy,
    ffmpeg: Arc<Ffmpeg>,
    explicit: HashMap<String, String>,
    hls_concurrency: usize,
}

impl MediaDownloader {
    /// 按运行配置构造。`transport` 决定超时/代理/网卡；`config` 决定分段与重试。
    pub fn new(ffmpeg: Arc<Ffmpeg>, config: &Config) -> Result<Self, DownloadError> {
        let transport = TransportConfig {
            connect_timeout: Some(config.connect_timeout),
            read_timeout: Some(config.read_timeout),
            proxy: config.proxy.clone(),
            no_proxy: config.no_proxy,
            interface: config.interface.clone(),
            dns: config.dns.clone(),
        };
        let client = WreqSegmentDownloader::with_config(&transport, None)
            .map_err(|error| DownloadError::Network(format!("下载客户端初始化失败: {error}")))?;
        let (global, host) = config.budget_limits();
        let policy = crate::transport::segment::TransferPolicy {
            budget: crate::transfer::budget::ConnectionBudget::new(global, host),
            backoff: crate::transfer::backoff::BackoffPolicy::default(),
            durability: Arc::new(crate::transport::segment::FileSyncDurability),
        };
        Ok(Self {
            client,
            config: RangeDownloaderConfig {
                connections: config.connections,
                retry_attempts: config.retry_attempts,
                retry_wait: config.retry_wait,
                sleep: Arc::new(|duration| Box::pin(tokio::time::sleep(duration))),
            },
            policy,
            ffmpeg,
            explicit: config.headers.clone(),
            hls_concurrency: hls_concurrency(config),
        })
    }

    fn range(&self, page_url: Option<&str>) -> RangeDownloader {
        RangeDownloader::with_policy(
            Arc::new(self.client.for_page(page_url.map(str::to_string))),
            self.config.clone(),
            self.policy.clone(),
        )
    }

    /// 头来源：显式（任务自身 > 全局）打底，自动推断留给传输层按目标 URL 重算。
    fn sources(&self, headers: &HashMap<String, String>) -> HeaderSources {
        let mut explicit = self.explicit.clone();
        for (name, value) in headers {
            insert_ci(&mut explicit, name, value.clone());
        }
        HeaderSources::from_explicit(explicit)
    }

    async fn transfer(
        &self,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        context: &DownloadContext,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        let sources = self.sources(headers);
        let page_url = context.page_url.as_deref();
        let track = TransferTrack::new(
            context
                .task_id
                .clone()
                .unwrap_or_else(|| dest.to_string_lossy().into_owned()),
            context.asset_id.clone().unwrap_or_else(|| "file".into()),
        );
        let mut report = self
            .range(page_url)
            .download_tracked(
                &track,
                url,
                dest,
                &sources.static_headers(),
                page_url,
                progress,
            )
            .await?;

        // 内容识别 HLS：传输层只负责字节，播放列表要交给 HLS 落地器分片封装。
        let mut prefix = [0u8; 16];
        let mut file = tokio::fs::File::open(dest).await.map_err(map_fs_error)?;
        let n = file.read(&mut prefix).await.map_err(map_fs_error)?;
        drop(file);
        if !crate::hls::is_hls_content(&String::from_utf8_lossy(&prefix[..n])) {
            return Ok(report);
        }
        let raw = HlsTransfer {
            owner: self,
            track: &track,
            page_url,
            sources: &sources,
            progress,
            network: AtomicU64::new(0),
            attempts: AtomicU64::new(0),
        };
        HlsMaterializer {
            downloader: &raw,
            ffmpeg: &self.ffmpeg,
            sources: &sources,
            page_url,
            concurrency: self.hls_concurrency,
            budget: None,
        }
        .materialize(url, dest)
        .await
        .map_err(|error| {
            // 播放列表此刻还躺在目标路径上：封装失败必须清掉，否则调用方会拿到
            // 一个 16KB 的 m3u8 当成「下载成功」。
            let _ = std::fs::remove_file(dest);
            match error {
                HlsFailure::Download(error) => error,
                HlsFailure::Io(error) => DownloadError::Io(error),
                HlsFailure::EmptyOutput => {
                    DownloadError::OutputInvalid("封装产物为空，不能交付".into())
                }
                other => DownloadError::Protocol(other.to_string()),
            }
        })?;
        report.network_bytes += raw.network.load(Ordering::Relaxed);
        report.attempts += raw.attempts.load(Ordering::Relaxed) as u32;
        report.validated_bytes = tokio::fs::metadata(dest).await.map_err(map_fs_error)?.len();
        report.total_bytes = Some(report.validated_bytes);
        Ok(report)
    }
}

#[async_trait]
impl FileDownloader for MediaDownloader {
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
    ) -> Result<u64, DownloadError> {
        self.download_with_progress(url, dest, headers, None).await
    }

    async fn download_with_progress(
        &self,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<u64, DownloadError> {
        Ok(self
            .transfer(url, dest, headers, &DownloadContext::default(), progress)
            .await?
            .validated_bytes)
    }

    async fn download_tracked(
        &self,
        track: &TransferTrack,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        page_url: Option<&str>,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        let context = DownloadContext {
            page_url: page_url.map(str::to_string),
            task_id: Some(track.task_id.clone()),
            asset_id: Some(track.asset_id.clone()),
        };
        self.transfer(url, dest, headers, &context, progress).await
    }

    async fn download_with_context(
        &self,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        context: &DownloadContext,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        self.transfer(url, dest, headers, context, progress).await
    }
}

/// HLS 内部只拉原始索引/分片，禁止递归把索引再次解释成整部视频。
struct HlsTransfer<'a> {
    owner: &'a MediaDownloader,
    track: &'a TransferTrack,
    page_url: Option<&'a str>,
    sources: &'a HeaderSources,
    progress: Option<&'a dyn ProgressSink>,
    network: AtomicU64,
    attempts: AtomicU64,
}

#[async_trait]
impl FileDownloader for HlsTransfer<'_> {
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        _: &HashMap<String, String>,
    ) -> Result<u64, DownloadError> {
        let report = self
            .owner
            .range(self.page_url)
            .download_tracked(
                self.track,
                url,
                dest,
                &self.sources.static_headers(),
                self.page_url,
                self.progress,
            )
            .await?;
        self.network
            .fetch_add(report.network_bytes, Ordering::Relaxed);
        self.attempts
            .fetch_add(report.attempts as u64, Ordering::Relaxed);
        Ok(report.validated_bytes)
    }

    async fn download_tracked(
        &self,
        track: &TransferTrack,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        page_url: Option<&str>,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        let mut headers = headers.clone();
        for (name, value) in self.sources.static_headers() {
            insert_ci(&mut headers, &name, value);
        }
        let report = self
            .owner
            .range(page_url.or(self.page_url))
            .download_tracked(
                track,
                url,
                dest,
                &headers,
                page_url.or(self.page_url),
                progress.or(self.progress),
            )
            .await?;
        self.network
            .fetch_add(report.network_bytes, Ordering::Relaxed);
        self.attempts
            .fetch_add(report.attempts as u64, Ordering::Relaxed);
        Ok(report)
    }
}
