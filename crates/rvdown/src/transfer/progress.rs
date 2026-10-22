//! 传输进度事件：任务/资产/尝试三维可区分，**重试不假涨进度**，网速只算真实网络字节。
//!
//! 三个字节口径分开记，避免常见的两个坑：
//! - 进度事件不带任务标识时，批量/音频/重试的进度会搅在一起；
//! - 把重试的字节也累加进"已传输"，能超过 total，百分比会假涨过 100%。
//!
//! 因此这里把「已验证有效字节」「断点恢复字节」「本次网络字节」拆开记。

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 传输阶段：看板与日志用它区分「还在探测 / 正在传 / 合并中 / 检验中」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum TransferPhase {
    #[default]
    Probing,
    Downloading,
    Merging,
    Verifying,
    Done,
    Failed,
}

impl TransferPhase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Probing => "探测",
            Self::Downloading => "下载中",
            Self::Merging => "合并中",
            Self::Verifying => "检验中",
            Self::Done => "完成",
            Self::Failed => "失败",
        }
    }

    /// 稳定机器码：JSON 协议（`progress.phase`）只用它，`label` 只给人看。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Probing => "probing",
            Self::Downloading => "downloading",
            Self::Merging => "merging",
            Self::Verifying => "verifying",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// 一次传输的身份标识：批量里靠它区分任务与资产（视频/音频/封面/分片）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferTrack {
    pub task_id: String,
    pub asset_id: String,
}

impl TransferTrack {
    pub fn new(task_id: impl Into<String>, asset_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            asset_id: asset_id.into(),
        }
    }
}

/// 单次传输进度事件。**进度不变量**见 `validated_bytes` / `network_bytes` 的注释。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferEvent {
    pub task_id: String,
    pub asset_id: String,
    /// 第几次尝试（从 1 起）；重试不假涨 `validated_bytes`。
    pub attempt_id: u32,
    pub phase: TransferPhase,
    /// **已验证有效**累计字节：只有真正落盘并（分段）标记完成的字节才计入，
    /// 重试同一段不重复计入，因此百分比永不假涨过 100%。
    pub validated_bytes: u64,
    /// 本次从断点恢复的字节（不计入网速）。
    pub resumed_bytes: u64,
    /// 本次真实网络传输的字节（**只有它**计入网速）。
    pub network_bytes: u64,
    /// 资源总长度；拿不到时为 None（看板显示「—」，不编百分比/ETA）。
    pub total_bytes: Option<u64>,
}

impl TransferEvent {
    /// 百分比（0-100）：总量未知时返回 None；完成前绝不谎报 100%。
    pub fn percent(&self) -> Option<u8> {
        let total = self.total_bytes?;
        if total == 0 {
            return Some(100);
        }
        let raw = self.validated_bytes.min(total) as f64 * 100.0 / total as f64;
        Some(raw.floor() as u8)
    }
}

/// 一次传输结束后的汇总：调用方按 `network_bytes` 算真实网速。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferReport {
    /// 本次真实网络字节（算网速）。
    pub network_bytes: u64,
    /// 断点恢复的字节（不算网速）。
    pub resumed_bytes: u64,
    /// 有效累计字节（= 产物大小）。
    pub validated_bytes: u64,
    /// 实际使用的尝试次数（含首次）。
    pub attempts: u32,
    pub total_bytes: Option<u64>,
}

impl TransferReport {
    /// 老实现（只返回一次传输的字节数）合成的最小汇报。
    pub fn from_single_attempt(track: &TransferTrack, bytes: u64) -> Self {
        let _ = track;
        Self {
            network_bytes: bytes,
            resumed_bytes: 0,
            validated_bytes: bytes,
            attempts: 1,
            total_bytes: None,
        }
    }
}

/// 进度出口的事件入口：默认适配到旧 `on_transfer(validated, total)`，老实现零改动。
pub trait ProgressEventSink: Send + Sync {
    fn on_event(&self, event: &TransferEvent);
}

/// 网速滑窗（近 `window` 秒加权），只吃真实网络字节。
///
#[derive(Debug)]
pub struct RateMeter {
    window: Duration,
    samples: Mutex<Vec<(Instant, u64)>>,
}

impl RateMeter {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            samples: Mutex::new(Vec::new()),
        }
    }

    /// 记一次真实网络字节增量（恢复字节不调用本方法）。
    pub fn record(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let mut samples = self.samples.lock().unwrap();
        samples.push((Instant::now(), bytes));
        self.trim(&mut samples);
    }

    /// 当前速率（字节/秒）；窗口内没有样本返回 None（看板显示「—」）。
    pub fn bytes_per_second(&self) -> Option<f64> {
        let mut samples = self.samples.lock().unwrap();
        self.trim(&mut samples);
        if samples.is_empty() {
            return None;
        }
        let first = samples.first()?.0;
        // 窗口刚开时 elapsed 可能是 0：夹到 1ms，给出瞬时估计而不是「—」。
        // 速率会随窗口推进自然收敛，不会一直虚高。
        let elapsed = first.elapsed().as_secs_f64().max(0.001);
        let total: u64 = samples.iter().map(|(_, bytes)| bytes).sum();
        Some(total as f64 / elapsed)
    }

    fn trim(&self, samples: &mut Vec<(Instant, u64)>) {
        let cutoff = Instant::now() - self.window;
        samples.retain(|(at, _)| *at >= cutoff);
    }
}

/// 单任务的文字看板行数据（TTY/非 TTY 共用同一批字段，只差渲染）。
#[derive(Debug, Clone, Default)]
pub struct TaskProgress {
    pub task_id: String,
    pub asset_id: String,
    pub label: String,
    pub validated_bytes: u64,
    pub total_bytes: Option<u64>,
    pub phase: TransferPhase,
}

impl TaskProgress {
    pub fn new(track: &TransferTrack, label: impl Into<String>) -> Self {
        Self {
            task_id: track.task_id.clone(),
            asset_id: track.asset_id.clone(),
            label: label.into(),
            validated_bytes: 0,
            total_bytes: None,
            phase: TransferPhase::Probing,
        }
    }

    /// 从事件就地更新；**只涨不跌**（重试不把有效进度往回拨）。
    pub fn apply(&mut self, event: &TransferEvent) {
        if event.task_id != self.task_id {
            return;
        }
        self.validated_bytes = self.validated_bytes.max(event.validated_bytes);
        self.total_bytes = event.total_bytes.or(self.total_bytes);
        self.phase = event.phase;
    }
}

/// 人类可读的字节数（`28.0 MB` / `936 B`）。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// 人类可读的速率（`3.2 MB/s`）；None 显示「—」。
pub fn format_rate(bytes_per_second: Option<f64>) -> String {
    match bytes_per_second {
        Some(rate) if rate > 0.0 => format!("{}/s", format_bytes(rate as u64)),
        _ => "—".to_string(),
    }
}

/// ETA（秒）：只在总量可知且速率 > 0 时给，否则「—」。
pub fn eta_seconds(validated: u64, total: Option<u64>, rate: Option<f64>) -> Option<u64> {
    let total = total?;
    let rate = rate.filter(|value| *value > 0.0)?;
    let remaining = total.saturating_sub(validated) as f64;
    Some((remaining / rate).ceil() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(validated: u64, network: u64, total: Option<u64>) -> TransferEvent {
        TransferEvent {
            task_id: "t1".into(),
            asset_id: "video".into(),
            attempt_id: 1,
            phase: TransferPhase::Downloading,
            validated_bytes: validated,
            resumed_bytes: 0,
            network_bytes: network,
            total_bytes: total,
        }
    }

    #[test]
    fn 百分比永不假涨过100() {
        // 重试把网络字节累加到超过 total，也不允许百分比假涨。
        let mut e = event(300, 900, Some(300));
        assert_eq!(e.percent(), Some(100));
        e.validated_bytes = 500;
        assert_eq!(e.percent(), Some(100), "有效字节封顶 total");
        let unknown = event(300, 300, None);
        assert_eq!(unknown.percent(), None, "总量未知不编百分比");
    }

    #[test]
    fn 有效字节与网络字节分开记() {
        // 续传恢复 1M + 本次网络只下了 256K：网速按 256K 算，进度按 1.25M 算。
        let e = TransferEvent {
            resumed_bytes: 1024 * 1024,
            ..event(1024 * 1024 + 256 * 1024, 256 * 1024, Some(2 * 1024 * 1024))
        };
        assert_eq!(e.validated_bytes, 1024 * 1024 + 256 * 1024);
        assert_eq!(e.network_bytes, 256 * 1024, "只有真实网络字节算网速");
        assert_eq!(e.resumed_bytes, 1024 * 1024);
        assert_eq!(e.percent(), Some(62));
    }

    #[test]
    fn 多任务进度按task_id分开() {
        let mut a = TaskProgress::new(&TransferTrack::new("t1", "video"), "A.mp4");
        let mut b = TaskProgress::new(&TransferTrack::new("t2", "video"), "B.mp4");
        a.apply(&event(100, 100, Some(200)));
        let mut other = event(50, 50, Some(200));
        other.task_id = "t2".into();
        b.apply(&other);
        assert_eq!(a.validated_bytes, 100);
        assert_eq!(b.validated_bytes, 50);
        // 另一个任务的事件不能串到 t1 上。
        a.apply(&other);
        assert_eq!(a.validated_bytes, 100);
    }

    #[test]
    fn 进度只涨不跌() {
        let mut task = TaskProgress::new(&TransferTrack::new("t1", "video"), "A.mp4");
        task.apply(&event(400, 400, Some(1000)));
        task.apply(&event(100, 100, Some(1000)));
        assert_eq!(task.validated_bytes, 400, "重试不把有效进度往回拨");
    }

    #[test]
    fn 网速只吃真实网络字节() {
        let meter = RateMeter::new(Duration::from_secs(5));
        assert_eq!(meter.bytes_per_second(), None, "没有样本显示「—」");
        meter.record(0);
        assert_eq!(meter.bytes_per_second(), None);
        meter.record(4096);
        std::thread::sleep(Duration::from_millis(10));
        let rate = meter.bytes_per_second().expect("有样本就有速率");
        assert!(rate > 0.0, "真实网络字节进速率");
    }

    #[test]
    fn 可读格式与eta只在可知时给() {
        assert_eq!(format_bytes(936), "936 B");
        assert_eq!(format_bytes(28 * 1024 * 1024), "28.0 MB");
        assert_eq!(format_rate(None), "—");
        assert_eq!(format_rate(Some(0.0)), "—");
        assert_eq!(format_rate(Some(3.0 * 1024.0 * 1024.0)), "3.0 MB/s");
        assert_eq!(eta_seconds(0, Some(100), Some(10.0)), Some(10));
        assert_eq!(eta_seconds(0, None, Some(10.0)), None, "总量未知不给 ETA");
        assert_eq!(eta_seconds(0, Some(100), None), None, "没有速率不给 ETA");
        assert_eq!(eta_seconds(0, Some(100), Some(0.0)), None);
    }
}
