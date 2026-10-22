//! 错误分类与**封闭错误码表**。
//!
//! 错误码是跨语言契约：Python / Node 调用方只准解析 `code`，`message` 是固定英文短句，
//! `detail` 只用于排查（可能含上游原始文案）。新增码只能追加，既有码的语义不改。

use serde::{Deserialize, Serialize};

/// 失败分类：fs 永久错误（磁盘/权限/路径/IO）绝不重试，其余按瞬时故障策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadFailureKind {
    DiskFull,
    Permission,
    Path,
    Io,
    Network,
    Http,
    /// 协议层异常（区间不符、缺 Content-Range、服务器不支持分段等）。
    Protocol,
    /// 资源在传输过程中发生变化（验证器失配 / 416），需要重新探测。
    ResourceChanged,
    /// 同一目标正被另一个任务/进程写入。
    Busy,
    Unknown,
}

impl DownloadFailureKind {
    /// 文件系统永久错误：换网络栈/重试都无意义。
    pub fn is_permanent(self) -> bool {
        matches!(
            self,
            DownloadFailureKind::DiskFull
                | DownloadFailureKind::Permission
                | DownloadFailureKind::Path
                | DownloadFailureKind::Io
        )
    }

    /// 瞬时 HTTP 故障可重试：网络错误、5xx/408/425/429；直链 403 也重试
    /// （CDN 防盗链/频控/签名令牌的瞬时拒绝）。其余 4xx 是确定性失败不空转。
    pub fn is_retryable_http_status(status: u16) -> bool {
        matches!(status, 403 | 408 | 425 | 429 | 500..=599)
    }
}

/// 跨语言错误码（JSON 中的 `code`）。**只增不改**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// 成功：产物已落盘并通过校验。
    Ok,
    /// 成功但成片没有音频轨（DASH 之外的单流常见）：需要人工确认。
    OkNoAudio,
    /// 任务本身非法（缺 url / 路径为空 / 行不是 JSON 对象）：未执行。
    InvalidJob,
    /// 404 / 410。
    HttpNotFound,
    /// 403：防盗链、频控或签名过期。
    HttpForbidden,
    /// 429。
    HttpRateLimited,
    /// 5xx。
    HttpServerError,
    /// 其它 4xx。
    HttpClientError,
    /// 连接失败、重置、TLS 握手失败等网络层错误。
    NetworkError,
    /// 连接/读取超时。
    NetworkTimeout,
    /// 协议异常：区间不符、缺 Content-Range、服务器不支持分段且单流也失败等。
    ProtocolError,
    /// 传输中资源发生变化（验证器/长度/重定向目标变化），已按有限重启策略重下仍失败。
    ResourceChanged,
    /// 目标文件正被另一个任务写入。
    TargetBusy,
    /// 磁盘空间不足（永久，不重试）。
    DiskFull,
    /// 没有写入权限（永久）。
    PermissionDenied,
    /// 路径无效（永久）。
    BadPath,
    /// 其它文件系统错误（永久）。
    IoError,
    /// 需要 ffmpeg 但找不到可执行文件。
    FfmpegMissing,
    /// ffmpeg 合并失败。
    MergeFailed,
    /// 产物小于可信下限（疑似错误页/空文件）。
    OutputTooSmall,
    /// 产物没有视频轨（可能是中断残留的坏文件）。
    OutputNoVideoTrack,
    /// 产物为空或无法读取。
    OutputInvalid,
    /// 进程被中断（SIGINT/SIGTERM），已完成部分照常落盘。
    Cancelled,
}

impl ErrorCode {
    /// 固定英文短句：调用方可直接展示，不解析。
    pub fn description(self) -> &'static str {
        match self {
            ErrorCode::Ok => "download finished",
            ErrorCode::OkNoAudio => "download finished, but the output has no audio track",
            ErrorCode::InvalidJob => "invalid job definition",
            ErrorCode::HttpNotFound => "resource not found (404/410)",
            ErrorCode::HttpForbidden => "access denied (403)",
            ErrorCode::HttpRateLimited => "rate limited (429)",
            ErrorCode::HttpServerError => "server error (5xx)",
            ErrorCode::HttpClientError => "request rejected (4xx)",
            ErrorCode::NetworkError => "network error",
            ErrorCode::NetworkTimeout => "network timeout",
            ErrorCode::ProtocolError => "protocol error (range/content-range)",
            ErrorCode::ResourceChanged => "resource changed during transfer",
            ErrorCode::TargetBusy => "target file is locked by another task",
            ErrorCode::DiskFull => "disk full",
            ErrorCode::PermissionDenied => "write permission denied",
            ErrorCode::BadPath => "invalid path",
            ErrorCode::IoError => "filesystem error",
            ErrorCode::FfmpegMissing => "ffmpeg executable not found",
            ErrorCode::MergeFailed => "audio/video merge failed",
            ErrorCode::OutputTooSmall => "output smaller than the plausible minimum",
            ErrorCode::OutputNoVideoTrack => "output has no video track",
            ErrorCode::OutputInvalid => "output is empty or unreadable",
            ErrorCode::Cancelled => "cancelled",
        }
    }

    /// 调用方稍后重试同一任务**可能**成功（不代表值得立即重试）。
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            ErrorCode::HttpForbidden
                | ErrorCode::HttpRateLimited
                | ErrorCode::HttpServerError
                | ErrorCode::NetworkError
                | ErrorCode::NetworkTimeout
                | ErrorCode::ProtocolError
                | ErrorCode::ResourceChanged
                | ErrorCode::TargetBusy
                | ErrorCode::Cancelled
        )
    }

    /// HTTP 状态码 → 错误码（只有 4xx/5xx 会走到这里）。
    pub fn from_http_status(status: u16) -> Self {
        match status {
            404 | 410 => ErrorCode::HttpNotFound,
            403 => ErrorCode::HttpForbidden,
            429 => ErrorCode::HttpRateLimited,
            500..=599 => ErrorCode::HttpServerError,
            _ => ErrorCode::HttpClientError,
        }
    }
}

impl From<DownloadFailureKind> for ErrorCode {
    fn from(kind: DownloadFailureKind) -> Self {
        match kind {
            DownloadFailureKind::DiskFull => ErrorCode::DiskFull,
            DownloadFailureKind::Permission => ErrorCode::PermissionDenied,
            DownloadFailureKind::Path => ErrorCode::BadPath,
            DownloadFailureKind::Io => ErrorCode::IoError,
            DownloadFailureKind::Network => ErrorCode::NetworkError,
            DownloadFailureKind::Http => ErrorCode::HttpClientError,
            DownloadFailureKind::Protocol => ErrorCode::ProtocolError,
            DownloadFailureKind::ResourceChanged => ErrorCode::ResourceChanged,
            DownloadFailureKind::Busy => ErrorCode::TargetBusy,
            DownloadFailureKind::Unknown => ErrorCode::ProtocolError,
        }
    }
}

/// 传输/落盘失败。`Display` 的中文只进 `detail`，不外露为契约。
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// 未进一步细分的失败：`detail` 说明原因。
    #[error("下载失败: {0}")]
    Failed(String),
    /// 网络层错误（连接失败、重置、TLS 等）。
    #[error("网络错误: {0}")]
    Network(String),
    /// 请求超时。
    #[error("请求超时")]
    Timeout,
    /// 协议异常（区间不符/缺 Content-Range/不支持分段等）。
    #[error("协议错误: {0}")]
    Protocol(String),
    /// 资源在传输中变化，有限重启后仍失败。
    #[error("资源变化: {0}")]
    ResourceChanged(String),
    /// 同一目标正被另一个任务写入。
    #[error("目标被占用: {0}")]
    Busy(String),
    /// 带状态码的下载失败：上层按 status 区分「直链 403 等瞬时拒绝可重试」与
    /// 「404/410 确定性失败不空转」。
    #[error("HTTP {status}: {detail}")]
    Http { status: u16, detail: String },
    /// 需要 ffmpeg 但找不到可执行文件。
    #[error("缺少 ffmpeg: {0}")]
    FfmpegMissing(String),
    /// 音视频合并失败。
    #[error("合并失败: {0}")]
    MergeFailed(String),
    /// 产物为空或无法读取。
    #[error("产物无效: {0}")]
    OutputInvalid(String),
    #[error("磁盘空间不足")]
    DiskFull,
    #[error("没有写入权限: {0}")]
    Permission(String),
    #[error("路径无效: {0}")]
    Path(String),
    #[error("文件系统错误: {0}")]
    Io(String),
}

impl DownloadError {
    pub fn kind(&self) -> DownloadFailureKind {
        match self {
            DownloadError::DiskFull => DownloadFailureKind::DiskFull,
            DownloadError::Permission(_) => DownloadFailureKind::Permission,
            DownloadError::Path(_) => DownloadFailureKind::Path,
            DownloadError::Io(_) => DownloadFailureKind::Io,
            DownloadError::Network(_) => DownloadFailureKind::Network,
            DownloadError::Timeout => DownloadFailureKind::Network,
            DownloadError::Protocol(_) => DownloadFailureKind::Protocol,
            DownloadError::ResourceChanged(_) => DownloadFailureKind::ResourceChanged,
            DownloadError::Busy(_) => DownloadFailureKind::Busy,
            DownloadError::OutputInvalid(_) => DownloadFailureKind::Protocol,
            DownloadError::Http { .. } => DownloadFailureKind::Http,
            // 合并类失败与网络/磁盘无关，既不永久也不按瞬时重试放大
            DownloadError::FfmpegMissing(_) | DownloadError::MergeFailed(_) => {
                DownloadFailureKind::Unknown
            }
            DownloadError::Failed(_) => DownloadFailureKind::Unknown,
        }
    }

    /// 稳定错误码：`Http` 按状态码细分，其余按失败分类。
    pub fn code(&self) -> ErrorCode {
        match self {
            DownloadError::Http { status, .. } => ErrorCode::from_http_status(*status),
            DownloadError::Timeout => ErrorCode::NetworkTimeout,
            DownloadError::FfmpegMissing(_) => ErrorCode::FfmpegMissing,
            DownloadError::MergeFailed(_) => ErrorCode::MergeFailed,
            DownloadError::OutputInvalid(_) => ErrorCode::OutputInvalid,
            other => ErrorCode::from(other.kind()),
        }
    }

    /// 瞬时 HTTP 拒绝（403/408/425/429/5xx）：退避后原链接重试通常能过；
    /// 其余 4xx 是确定性失败，不空转。
    pub fn is_retryable_http(&self) -> bool {
        match self {
            DownloadError::Http { status, .. } => {
                DownloadFailureKind::is_retryable_http_status(*status)
            }
            _ => false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("无法合并音视频: {0}")]
    Failed(String),
    #[error("缺少 ffmpeg，无法合并音视频")]
    MissingBinary,
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("无法检验下载产物: {0}")]
    Failed(String),
    #[error("缺少 ffmpeg，无法检验下载产物")]
    MissingBinary,
}
