//! 连接预算：全局 + 单 host 两层 FIFO 信号量，防「多任务 × 分段 × HLS」不受控放大。
//!
//! 预算层是**天花板**：典型 `3 任务 × 8 段 = 24 ≤ 32`、HLS 16 ≤ 32 都不触发排队，
//! 只有叠加时才限流。等待顺序是 FIFO（tokio::sync::Semaphore 公平）。
//!
//! **取用顺序是「先单 host、后全局」**，与直觉相反，这是刻意的：
//! 若先取全局再取单 host，一堆等同一 host 名额的任务会先把全局许可占满，
//! 自己卡在 host 队列上空转，把其它 host 的任务整体饿死（队头阻塞）。
//! 先取 host 意味着「在等某台 CDN 的任务」不持有全局名额，其它 host 照常推进。
//! 取用顺序 host → global，释放顺序 global → host，顺序一致所以不会死锁。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 默认全局连接预算：32。典型 3 任务 × 8 段 = 24 不受影响。
pub const DEFAULT_GLOBAL_CONNECTIONS: usize = crate::config::DEFAULT_GLOBAL_CONNECTIONS;
/// 默认单 host 连接预算：16，防打爆一台 CDN。
pub const DEFAULT_HOST_CONNECTIONS: usize = crate::config::DEFAULT_HOST_CONNECTIONS;

pub const MAX_GLOBAL_CONNECTIONS: usize = 128;
pub const MAX_HOST_CONNECTIONS: usize = 64;

/// 全局 + 单 host 连接预算。
#[derive(Debug)]
pub struct ConnectionBudget {
    global: Arc<Semaphore>,
    host_limit: usize,
    hosts: std::sync::Mutex<HashMap<String, Arc<Semaphore>>>,
}

/// 一次连接名额；Drop 时按反序释放（先全局后 host，与取用顺序 host → global 相反）。
#[derive(Debug)]
pub struct BudgetGuard {
    // 字段按声明顺序 drop：global 先释放、host 后释放，与取用顺序相反。
    global: Option<OwnedSemaphorePermit>,
    host: Option<OwnedSemaphorePermit>,
}

impl ConnectionBudget {
    pub fn new(global_limit: usize, host_limit: usize) -> Arc<Self> {
        Arc::new(Self {
            global: Arc::new(Semaphore::new(global_limit.max(1))),
            host_limit: host_limit.max(1),
            hosts: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// 不限流（单元测试用）。取值要低于 tokio 信号量的 MAX_PERMITS 上限。
    pub fn unlimited() -> Arc<Self> {
        Self::new(u32::MAX as usize, u32::MAX as usize)
    }

    fn host_semaphore(&self, host: &str) -> Arc<Semaphore> {
        let key = host.to_ascii_lowercase();
        let mut hosts = self.hosts.lock().unwrap();
        hosts
            .entry(key)
            .or_insert_with(|| Arc::new(Semaphore::new(self.host_limit)))
            .clone()
    }

    /// 取一个连接名额：**先单 host、后全局**（释放反序）。
    ///
    /// 反过来（先全局后 host）会让「等同一 host 的任务」占满全局名额并在 host 队列上
    /// 空转，把其它 host 的任务整体饿死——见模块注释。
    pub async fn acquire(self: &Arc<Self>, host: &str) -> BudgetGuard {
        let host_semaphore = self.host_semaphore(host);
        let host_permit = host_semaphore
            .acquire_owned()
            .await
            .expect("host 预算信号量不会关闭");
        let global = self
            .global
            .clone()
            .acquire_owned()
            .await
            .expect("全局预算信号量不会关闭");
        BudgetGuard {
            global: Some(global),
            host: Some(host_permit),
        }
    }

    /// 当前剩余的全局名额（排查用）。
    pub fn global_available(&self) -> usize {
        self.global.available_permits()
    }
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        // 释放顺序与取用相反：先全局、后 host。
        self.global.take();
        self.host.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn 全局预算不超限且默认值不改变原有并发语义() {
        assert_eq!(DEFAULT_GLOBAL_CONNECTIONS, 32);
        const {
            // 3 任务 × 8 段 = 24、HLS 16 都应落在默认预算内，不影响原有并发语义。
            assert!(DEFAULT_GLOBAL_CONNECTIONS >= 3 * 8);
            assert!(DEFAULT_GLOBAL_CONNECTIONS >= 16);
        }
    }

    #[tokio::test]
    async fn 超出全局预算的任务排队等待() {
        let budget = ConnectionBudget::new(1, 8);
        let held = budget.acquire("cdn.a").await;
        assert_eq!(budget.global_available(), 0);

        let acquired = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&acquired);
        let waiter = {
            let budget = Arc::clone(&budget);
            tokio::spawn(async move {
                let _guard = budget.acquire("cdn.a").await;
                counter.fetch_add(1, Ordering::SeqCst);
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            acquired.load(Ordering::SeqCst),
            0,
            "名额没释放前不能偷偷拿到"
        );
        drop(held);
        waiter.await.unwrap();
        assert_eq!(acquired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn 单host预算独立于全局() {
        let budget = ConnectionBudget::new(8, 1);
        let _a1 = budget.acquire("cdn.a").await;
        // 同 host 第二个要等；换 host 不受影响。
        let host_limited = {
            let budget = Arc::clone(&budget);
            tokio::spawn(async move {
                let _guard = budget.acquire("cdn.a").await;
                1
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!host_limited.is_finished(), "同 host 第二个连接必须排队");
        let _b = budget.acquire("cdn.b").await;
        assert!(!host_limited.is_finished(), "换 host 不占用 cdn.a 的名额");
    }

    #[tokio::test]
    async fn 等待顺序是先进先出() {
        let budget = ConnectionBudget::new(1, 8);
        let held = budget.acquire("cdn.a").await;
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for index in 0..3usize {
            let budget = Arc::clone(&budget);
            let order = Arc::clone(&order);
            tasks.push(tokio::spawn(async move {
                let _guard = budget.acquire("cdn.a").await;
                order.lock().unwrap().push(index);
            }));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(
            *order.lock().unwrap(),
            vec![0, 1, 2],
            "等待必须先进先出，不能让后来的任务插队"
        );
    }

    #[tokio::test]
    async fn 等同一host的任务不能占满全局名额饿死其它host() {
        // 反模式（先全局后 host）：3 个 cdn.a 任务把 2 个全局名额占满，其中 1 个卡在
        // host 队列上空转，cdn.b 的任务就被整体饿死。
        // 正确顺序（先 host 后全局）：等 cdn.a 的任务不持有全局名额，cdn.b 照常推进。
        let budget = ConnectionBudget::new(2, 1);
        let progressed = Arc::new(AtomicUsize::new(0));

        // 3 个任务挤同一台 host，只有 1 个能拿到 host 名额。
        let mut same_host = Vec::new();
        for _ in 0..3 {
            let budget = Arc::clone(&budget);
            let progressed = Arc::clone(&progressed);
            same_host.push(tokio::spawn(async move {
                let guard = budget.acquire("cdn.a").await;
                progressed.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(200)).await;
                drop(guard);
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 另一台 host 的任务必须能拿到名额，不被 cdn.a 的排队拖死。
        let other = {
            let budget = Arc::clone(&budget);
            let progressed = Arc::clone(&progressed);
            tokio::spawn(async move {
                let guard = budget.acquire("cdn.b").await;
                progressed.fetch_add(1, Ordering::SeqCst);
                drop(guard);
            })
        };
        let finished = tokio::time::timeout(Duration::from_millis(150), other).await;
        assert!(
            finished.is_ok(),
            "等同一 host 的任务不得占满全局名额把其它 host 饿死（当前已有 {} 个推进）",
            progressed.load(Ordering::SeqCst)
        );
        for task in same_host {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn 取用反序释放不自锁() {
        // 多个任务反复取用同一预算：反序释放保证不会把全局名额攥住导致自锁。
        let budget = ConnectionBudget::new(2, 2);
        let mut tasks = Vec::new();
        for index in 0..6usize {
            let budget = Arc::clone(&budget);
            tasks.push(tokio::spawn(async move {
                for _ in 0..5 {
                    let guard = budget.acquire(&format!("cdn.{}", index % 2)).await;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    drop(guard);
                }
            }));
        }
        let joined = tokio::time::timeout(Duration::from_secs(5), async {
            for task in tasks {
                task.await.unwrap();
            }
        })
        .await;
        assert!(joined.is_ok(), "预算取用不能死锁");
    }
}
