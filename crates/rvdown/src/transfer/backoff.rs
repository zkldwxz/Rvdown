//! 退避策略：`Retry-After` 优先，指数退避兜底；慢段走段级空闲超时而不是砍整文件。

use std::time::Duration;

/// 指数退避（`base × 2^(attempt-1)`，封顶 `max`）。attempt 从 1 起。
#[derive(Debug, Clone, Copy)]
pub struct BackoffPolicy {
    pub base: Duration,
    pub max: Duration,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        // 对齐 aria2 --retry-wait=2 的起步节奏，封顶 30s：大文件重试不会退到分钟级。
        Self {
            base: Duration::from_secs(2),
            max: Duration::from_secs(30),
        }
    }
}

impl BackoffPolicy {
    /// 第 `attempt` 次重试前的等待（attempt 从 1 起，表示「第一次失败之后」）。
    pub fn delay_for(&self, attempt: usize) -> Duration {
        let exponent = attempt.saturating_sub(1).min(16);
        let factor = 1u64 << exponent;
        self.base
            .checked_mul(factor as u32)
            .map(|value| value.min(self.max))
            .unwrap_or(self.max)
    }
}

/// `Retry-After` 头的上限：服务器说「等 1 小时」也不能把下载卡死。
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// 把 429/503 的 `Retry-After`（delta-seconds 或 HTTP-date）夹到 `0..=MAX_RETRY_AFTER`。
///
/// 只认 delta-seconds：HTTP-date 形态解析依赖当前时钟，夹不准时宁可退指数退避，
/// 也不按错的时间干等。
pub fn clamp_retry_after(raw: Option<Duration>) -> Option<Duration> {
    raw.map(|value| value.min(MAX_RETRY_AFTER))
}

/// 重试等待：`Retry-After` 优先，否则指数退避。
pub fn retry_delay(
    policy: &BackoffPolicy,
    attempt: usize,
    retry_after: Option<Duration>,
) -> Duration {
    if let Some(delay) = clamp_retry_after(retry_after) {
        return delay;
    }
    policy.delay_for(attempt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 指数退避封顶不会退到分钟级() {
        let policy = BackoffPolicy {
            base: Duration::from_secs(2),
            max: Duration::from_secs(30),
        };
        assert_eq!(policy.delay_for(1), Duration::from_secs(2));
        assert_eq!(policy.delay_for(2), Duration::from_secs(4));
        assert_eq!(policy.delay_for(3), Duration::from_secs(8));
        assert_eq!(policy.delay_for(5), Duration::from_secs(30));
        assert_eq!(policy.delay_for(99), Duration::from_secs(30), "封顶 30s");
    }

    #[test]
    fn retry_after优先于指数退避且被夹住() {
        let policy = BackoffPolicy::default();
        assert_eq!(
            retry_delay(&policy, 1, Some(Duration::from_secs(7))),
            Duration::from_secs(7),
            "服务器让等 7s 就等 7s，不自作主张"
        );
        assert_eq!(
            retry_delay(&policy, 1, Some(Duration::from_secs(3600))),
            MAX_RETRY_AFTER,
            "服务器说等一小时也要夹住，不能把下载卡死"
        );
        assert_eq!(
            retry_delay(&policy, 1, Some(Duration::ZERO)),
            Duration::ZERO,
            "Retry-After: 0 立即重试"
        );
        assert_eq!(retry_delay(&policy, 1, None), Duration::from_secs(2));
    }
}
