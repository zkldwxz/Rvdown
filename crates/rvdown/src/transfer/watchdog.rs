//! 持续慢速与空闲超时不同：只按本次真实网络字节判断，不把恢复字节当网速。

use crate::{
    error::DownloadError,
    traits::ProgressSink,
    transfer::progress::{TransferEvent, TransferPhase},
};
use std::{
    future::Future,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::Duration,
};

/// 可选慢连接策略。默认关闭，不能把正常的慢网络自动当成失败。
#[derive(Debug, Clone, Copy)]
pub struct LowSpeedPolicy {
    pub bytes_per_second: u64,
    /// 完整采样窗口；首次观察到下载阶段后再等待一个窗口，排除连接预热。
    pub window: Duration,
}

pub(crate) struct ObservedProgress<'a> {
    downstream: Option<&'a dyn ProgressSink>,
    network: AtomicU64,
    downloading: AtomicBool,
}

impl<'a> ObservedProgress<'a> {
    pub(crate) fn new(downstream: Option<&'a dyn ProgressSink>) -> Self {
        Self {
            downstream,
            network: AtomicU64::new(0),
            downloading: AtomicBool::new(false),
        }
    }
}

impl ProgressSink for ObservedProgress<'_> {
    fn on_item(&self, message: &str) {
        if let Some(sink) = self.downstream {
            sink.on_item(message);
        }
    }
    fn on_event(&self, event: &TransferEvent) {
        self.network
            .fetch_max(event.network_bytes, Ordering::Relaxed);
        self.downloading
            .store(event.phase == TransferPhase::Downloading, Ordering::Relaxed);
        if let Some(sink) = self.downstream {
            sink.on_event(event);
        }
    }
}

/// 只包裹字节传输，不包裹 ffmpeg/产物检验；取消后由既有 RAII 释放锁和预算。
pub(crate) async fn watch<T>(
    future: impl Future<Output = Result<T, DownloadError>>,
    progress: &ObservedProgress<'_>,
    policy: LowSpeedPolicy,
) -> Result<T, DownloadError> {
    let mut timer =
        tokio::time::interval_at(tokio::time::Instant::now() + policy.window, policy.window);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut previous: Option<(std::time::Instant, u64)> = None;
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result=&mut future => return result,
            _=timer.tick() => {
                if !progress.downloading.load(Ordering::Relaxed) {
                    previous=None;
                    continue;
                }
                let now=std::time::Instant::now();
                let bytes=progress.network.load(Ordering::Relaxed);
                if let Some((start,before))=previous {
                    let elapsed=now.duration_since(start).as_nanos().max(1);
                    let rate=((bytes.saturating_sub(before) as u128)*1_000_000_000/elapsed)
                        .min(u64::MAX as u128) as u64;
                    if rate<policy.bytes_per_second {
                        return Err(DownloadError::TooSlow { bytes_per_second:rate,
                            minimum_bytes_per_second:policy.bytes_per_second });
                    }
                }
                previous=Some((now,bytes));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::progress::{TransferEvent, TransferPhase};

    fn event(bytes: u64, resumed: u64, phase: TransferPhase) -> TransferEvent {
        TransferEvent {
            task_id: "task".into(),
            asset_id: "video".into(),
            attempt_id: 1,
            phase,
            network_bytes: bytes,
            resumed_bytes: resumed,
            validated_bytes: resumed + bytes,
            total_bytes: Some(64 * 1024 * 1024),
        }
    }
    fn policy() -> LowSpeedPolicy {
        LowSpeedPolicy {
            bytes_per_second: 8192,
            window: Duration::from_millis(40),
        }
    }

    #[tokio::test]
    async fn 一直有小块数据也必须识别持续慢速不能等到空闲超时() {
        let sink = ObservedProgress::new(None);
        let slow = async {
            for n in 1..1000 {
                sink.on_event(&event(n, 0, TransferPhase::Downloading));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        };
        let result = tokio::time::timeout(Duration::from_secs(2), watch(slow, &sink, policy()))
            .await
            .unwrap();
        assert!(matches!(result, Err(DownloadError::TooSlow { .. })));
    }

    #[tokio::test]
    async fn 大量恢复字节不能让慢速连接冒充高速() {
        let sink = ObservedProgress::new(None);
        let slow = async {
            for _ in 0..1000 {
                sink.on_event(&event(0, 50 * 1024 * 1024, TransferPhase::Downloading));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        };
        assert!(matches!(
            watch(slow, &sink, policy()).await,
            Err(DownloadError::TooSlow { .. })
        ));
    }

    #[tokio::test]
    async fn 正常下载和只做探测检验的耗时不能被当成持续慢速() {
        for phase in [
            TransferPhase::Downloading,
            TransferPhase::Probing,
            TransferPhase::Verifying,
        ] {
            let sink = ObservedProgress::new(None);
            let fast = async {
                for n in 1..25 {
                    sink.on_event(&event(n * 8192, 0, phase));
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Ok(123)
            };
            assert_eq!(watch(fast, &sink, policy()).await.unwrap(), 123);
        }
    }

    #[tokio::test]
    async fn 当前窗口曾经停止传输时重置采样不能误杀下一个资产() {
        let sink = ObservedProgress::new(None);
        let transfer = async {
            sink.on_event(&event(0, 0, TransferPhase::Downloading));
            tokio::time::sleep(Duration::from_millis(10)).await;
            sink.on_event(&event(0, 0, TransferPhase::Merging));
            tokio::time::sleep(Duration::from_millis(120)).await;
            Ok(())
        };
        assert!(watch(transfer, &sink, policy()).await.is_ok());
    }
}
