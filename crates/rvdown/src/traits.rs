//! 可注入的链路组件接口。测试注入假实现，生产注入默认实现。
//!
//! 调用方只需要 `FileDownloader`（纯文件落盘）与 `Client`（媒体任务）；
//! `AudioMuxer` / `VideoProbe` / `FrameGrabber` 给需要自己编排的调用方用。

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;

use crate::error::{DownloadError, MuxError, ProbeError};
use crate::transfer::progress::{TransferReport, TransferTrack};
use crate::types::VideoProbeResult;

/// 一次下载的上下文：原始内容页 URL 供每跳重算自动 Referer，任务标识供进度区分。
#[derive(Debug, Clone, Default)]
pub struct DownloadContext {
    /// 原始内容页 URL（`https://www.douyin.com/video/...` 这类）。
    /// 平台规则识别不了目标域时用它的 origin 兜底 Referer。
    pub page_url: Option<String>,
    /// 进度标识：批量下载里区分任务；单任务可省略。
    pub task_id: Option<String>,
    /// 进度标识：区分视频 / 音频 / 封面 / HLS 分片。
    pub asset_id: Option<String>,
}

impl DownloadContext {
    fn track(&self, dest: &Path) -> TransferTrack {
        TransferTrack::new(
            self.task_id
                .clone()
                .unwrap_or_else(|| dest.to_string_lossy().into_owned()),
            self.asset_id.clone().unwrap_or_else(|| "file".to_string()),
        )
    }
}

/// 把 url 落到 dest 的字节流组件。
#[async_trait]
pub trait FileDownloader: Send + Sync {
    /// 把 url 落到 dest（原子落盘：先 `.part` 再 rename），返回真实字节数。
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
    ) -> Result<u64, DownloadError>;

    /// 带任务/资产标识的下载入口：回报网络/恢复/有效字节与尝试次数。
    ///
    /// 默认实现只转调 `download_with_context`，因此简单实现只需写 `download`。
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
        self.download_with_context(url, dest, headers, &context, progress)
            .await
    }

    /// 可选字节进度出口。
    async fn download_with_progress(
        &self,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<u64, DownloadError> {
        let track = TransferTrack::new(dest.to_string_lossy(), "file");
        self.download_tracked(&track, url, dest, headers, None, progress)
            .await
            .map(|report| report.validated_bytes)
    }

    /// 完整入口：上下文（原始页/任务标识）+ 进度。
    async fn download_with_context(
        &self,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        context: &DownloadContext,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        let track = context.track(dest);
        self.download_tracked(
            &track,
            url,
            dest,
            headers,
            context.page_url.as_deref(),
            progress,
        )
        .await
    }
}

#[async_trait]
pub trait AudioMuxer: Send + Sync {
    /// 把 audio 合进 video，产物写到 dest（不覆盖入参文件）。
    /// 默认实现：`ffmpeg -i v -i a -map 0:v:0 -map 1:a:0 -c copy -movflags +faststart`。
    async fn mux(&self, video: &Path, audio: &Path, dest: &Path) -> Result<(), MuxError>;
}

#[async_trait]
pub trait VideoProbe: Send + Sync {
    /// 产物基础检验：`ffmpeg -i` 只读文件头（大小 / 视频轨 / 音频轨 / 时长）。
    async fn probe(&self, path: &Path) -> Result<VideoProbeResult, ProbeError>;
}

#[async_trait]
pub trait FrameGrabber: Send + Sync {
    /// 封面截帧兜底：本地视频第 at_seconds 秒截一帧。
    async fn capture_frame(
        &self,
        video: &Path,
        dest: &Path,
        at_seconds: f64,
    ) -> Result<(), ProbeError>;
}

/// 进度出口：字节事件 + 可选的人类可读行（日志）。
pub trait ProgressSink: Send + Sync {
    /// 人类可读日志（`开始下载: ...` 这类），CLI 打到 stderr。
    fn on_item(&self, message: &str) {
        let _ = message;
    }
    /// 字节事件。`validated_bytes` 是已验证有效字节（重试不假涨），
    /// `network_bytes` 是本次真实网络字节（只有它算网速）。
    fn on_event(&self, event: &crate::transfer::progress::TransferEvent) {
        let _ = event;
    }
}
