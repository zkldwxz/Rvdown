//! 内核数据类型：产物检验读数与阶段快照。

/// 产物基础检验结果（大小 / 是否含视频轨 / 音频轨 / 容器时长）。
#[derive(Debug, Clone, Default)]
pub struct VideoProbeResult {
    pub size_bytes: u64,
    pub has_video: bool,
    pub has_audio: bool,
    /// 容器里读到的真实时长（秒）；读不到时为空。
    pub duration_seconds: Option<f64>,
    /// 完全读不出流信息时的 stderr 摘要。
    pub detail: Option<String>,
}
