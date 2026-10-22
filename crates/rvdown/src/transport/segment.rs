//! 分段下载编排：`探测 → N 路 Range 并发 → .part 按偏移落盘 → 原子 rename`。
//!
//! 切分与续传语义对齐 aria2 的 `--split` / `--min-split-size` / `--file-allocation=none`；
//! 单流回退路径负责重试判定，产物一律经 `.part` 原子交付。
//!
//! 设计要点：
//! - `SegmentDownloader` 是换 HTTP 库的唯一接缝：wreq（Chrome 指纹伪装）只是默认实现，
//!   测试注入 Fake——禁网也能把编排逻辑测全。
//! - 编排不绑运行时：并发用 futures `buffer_unordered`（不用 tokio::spawn），
//!   退避 sleep 可注入；默认实现用 tokio，生产运行时由 CLI 提供。
//! - 失败分类是硬约束：fs 永久错（磁盘满/权限/路径）绝不重试；
//!   瞬时 HTTP（403/408/425/429/5xx）与网络抖动按 attempts 重试；404/410 不空转。
//! - 断点续传见 `control.rs`（台账 v2：资源身份 + 字节区间）：
//!   **数据先落盘再标记完成**、206 逐项校验、失败保留可信段、绝不错误交付旧 dest。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::StreamExt;
use futures::TryStreamExt;

use crate::error::{DownloadError, DownloadFailureKind};
use crate::traits::{FileDownloader, ProgressSink};
use crate::transfer::backoff::{retry_delay, BackoffPolicy};
use crate::transfer::budget::{BudgetGuard, ConnectionBudget};
use crate::transfer::progress::{TransferEvent, TransferPhase, TransferReport, TransferTrack};

use super::control::{
    decide_resume, discard_partial, keep_partial, part_path_for, ControlLedger, DestLock,
    RestartReason, ResumeDecision,
};
use super::identity::{resource_domain, ByteRange, ContentRange, ResourceIdentity};
use super::plan::{
    normalize_connections, plan_segments, SegmentRange, DEFAULT_SEGMENT_CONNECTIONS,
};

/// 资源变化最多重启一次（416 / 验证器失配后重新探测），绝不无限循环。
pub const MAX_RESOURCE_RESTARTS: u32 = 1;

/// 一次 HTTP 请求的字节流：编排层边读边写盘、边累计进度。
pub type ByteStream = Pin<Box<dyn futures::Stream<Item = Result<Vec<u8>, TransportError>> + Send>>;

/// 网络层错误（不含 HTTP 状态码——有响应就把状态码交给编排层判定）。
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("网络请求失败: {0}")]
    Network(String),
    #[error("请求超时")]
    Timeout,
}

/// HEAD 预检结果。
#[derive(Debug, Clone)]
pub struct HeadInfo {
    pub status: u16,
    pub content_length: Option<u64>,
    /// 响应带来的资源验证器（强 ETag / Last-Modified / 弱 ETag）。
    pub identity: ResourceIdentity,
    /// 服务器是否声明支持 Range（`Accept-Ranges: bytes`）。
    pub accept_ranges: bool,
}

/// GET 响应：状态码 + 字节流 + Content-Range + 资源验证器 + Retry-After。
pub struct SegmentResponse {
    pub status: u16,
    pub content_length: Option<u64>,
    pub body: ByteStream,
    /// `Content-Range` 解析结果；206 必须有，用于逐项校验请求区间。
    pub content_range: Option<ContentRange>,
    /// 响应带来的资源验证器。
    pub identity: ResourceIdentity,
    /// 429/503 的 `Retry-After`；其余状态为 None。
    pub retry_after: Option<Duration>,
}

/// HEAD 探测结果 + **跟随重定向后的最终响应地址**。
///
/// 资源域（断点续传身份）必须绑最终地址：同一个入口 URL 两次重定向到不同目标时，
/// 只绑入口会把两个不同资源当成同一个（RFC 9110 §8.8.1 + §7.6 有效请求 URI）。
pub struct ProbeOutcome {
    pub head: HeadInfo,
    /// 最终响应地址；None = 实现方不提供（调用方退回入口 URL）。
    pub effective_url: Option<String>,
}

/// GET 响应 + **该次请求**的最终响应地址（逐响应资源域校验用）。
pub struct ScopedResponse {
    pub response: SegmentResponse,
    /// 最终响应地址；None = 实现方不提供（调用方不做域校验）。
    pub effective_url: Option<String>,
}

/// `bytes=0-0` 范围探测结果（HEAD 被拒/无长度时的兜底），同样带最终响应地址。
pub struct RangeProbeOutcome {
    pub total: u64,
    pub identity: ResourceIdentity,
    pub effective_url: Option<String>,
}

/// 单段 HTTP 原语。换库（wreq ↔ 其它）只动默认实现，不动本 trait 与编排层。
#[async_trait]
pub trait SegmentDownloader: Send + Sync {
    /// HEAD 探测 + 最终响应地址。
    ///
    /// 默认实现只调用 `probe_head`（不知道最终地址）——既有实现零改动。
    /// 需要资源域绑定的实现（如 wreq 手工跟随重定向）覆盖本方法带回最终 URL；
    /// 必须按本次请求返回，**不能**用全局可变字段（并发任务会串身份）。
    async fn probe(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<ProbeOutcome, TransportError> {
        Ok(ProbeOutcome {
            head: self.probe_head(url, headers).await?,
            effective_url: None,
        })
    }

    /// `GET Range: bytes=0-0` 范围探测（HEAD 拿不到长度时用）。
    ///
    /// 默认实现复用 `get`（不知道最终地址）；覆盖时同样必须带回本次请求的最终 URL。
    async fn probe_range(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Option<RangeProbeOutcome> {
        let probe = SegmentRange {
            index: 0,
            start: 0,
            end: 0,
        };
        let response = self.get(url, Some(&probe), headers).await.ok()?;
        if response.status != 206 {
            return None;
        }
        let total = response
            .content_range
            .as_ref()
            .and_then(|range| range.total())?;
        if total == 0 {
            return None;
        }
        let mut identity = response.identity.clone();
        if identity.length == 0 {
            identity.length = total;
        }
        Some(RangeProbeOutcome {
            total,
            identity,
            effective_url: None,
        })
    }

    /// HEAD 预检；网络层失败返回 Err，拿到 HTTP 响应就 Ok（状态码由编排层判断）。
    async fn probe_head(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<HeadInfo, TransportError>;

    /// GET；`range` 为 None 表示不带 Range 头（单流整档）。
    async fn get(
        &self,
        url: &str,
        range: Option<&SegmentRange>,
        headers: &HashMap<String, String>,
    ) -> Result<SegmentResponse, TransportError>;

    /// 同 `get`，但带回**本次请求**跟随重定向后的最终地址。
    ///
    /// 为什么必须逐响应带：探测（HEAD/0-0）时跳到 A、之后某条 Range GET 跳到 B 时，
    /// A/B 可能同 size 同 ETag 但内容不同——只有每次响应的最终地址能发现这件事，
    /// 否则会把两条链路拼成交付文件（RFC 9110 §7.6 有效请求 URI）。
    ///
    /// 默认实现只调用 `get`（不知道最终地址，调用方跳过域校验）——既有实现零改动。
    /// 覆盖时同样必须按本次请求返回，**不能**用全局可变字段（并发任务会串身份）。
    async fn get_scoped(
        &self,
        url: &str,
        range: Option<&SegmentRange>,
        headers: &HashMap<String, String>,
    ) -> Result<ScopedResponse, TransportError> {
        Ok(ScopedResponse {
            response: self.get(url, range, headers).await?,
            effective_url: None,
        })
    }
}

/// `.part` 落盘后的持久化屏障：**先保证数据落盘，再标记完成**。
///
/// 崩溃后若台账先于数据持久化，就会把没写完的段当成已完成（错误交付）。
pub trait PartDurability: Send + Sync {
    fn sync_part(&self, file: &File) -> std::io::Result<()>;
}

/// 默认持久化屏障：`File::sync_data`（数据落盘）。
#[derive(Debug, Default, Clone, Copy)]
pub struct FileSyncDurability;

impl PartDurability for FileSyncDurability {
    fn sync_part(&self, file: &File) -> std::io::Result<()> {
        file.sync_data()
    }
}

/// 退避 sleep：测试注入即时返回的假 sleep，生产用 tokio。
pub type SleepFn = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

fn tokio_sleep() -> SleepFn {
    Arc::new(|duration: Duration| Box::pin(tokio::time::sleep(duration)))
}

/// 传输横切策略（additive）：连接预算、退避、`.part` 落盘屏障。
///
/// 与 `RangeDownloaderConfig` 分开是为了不给既有结构体加字段（避免广改调用点）；
/// 需要定制时用 `RangeDownloader::with_policy`。
#[derive(Clone)]
pub struct TransferPolicy {
    /// 全局 + 单 host 连接预算。
    pub budget: Arc<ConnectionBudget>,
    /// 退避策略（`Retry-After` 优先，指数退避兜底）。
    pub backoff: BackoffPolicy,
    /// `.part` 数据落盘屏障。
    pub durability: Arc<dyn PartDurability>,
}

impl Default for TransferPolicy {
    fn default() -> Self {
        Self {
            budget: ConnectionBudget::new(
                crate::transfer::budget::DEFAULT_GLOBAL_CONNECTIONS,
                crate::transfer::budget::DEFAULT_HOST_CONNECTIONS,
            ),
            backoff: BackoffPolicy::default(),
            durability: Arc::new(FileSyncDurability),
        }
    }
}

impl TransferPolicy {
    /// 单元测试用：不限流、零退避、真实落盘屏障。
    pub fn unbounded() -> Self {
        Self {
            budget: ConnectionBudget::unlimited(),
            backoff: BackoffPolicy {
                base: Duration::from_millis(0),
                max: Duration::from_millis(0),
            },
            durability: Arc::new(FileSyncDurability),
        }
    }
}

/// 编排配置：全部可注入，`Default` 即生产默认值。
#[derive(Clone)]
pub struct RangeDownloaderConfig {
    pub connections: usize,
    /// 尝试次数（含首次）；默认读 DOWNLOAD_RETRY_ATTEMPTS，回落 3。
    pub retry_attempts: usize,
    /// 两次重试的等待（aria2 --retry-wait 口径 2s）。
    pub retry_wait: Duration,
    pub sleep: SleepFn,
}

impl Default for RangeDownloaderConfig {
    fn default() -> Self {
        Self {
            connections: DEFAULT_SEGMENT_CONNECTIONS,
            retry_attempts: crate::config::DEFAULT_RETRY_ATTEMPTS,
            retry_wait: crate::config::DEFAULT_RETRY_WAIT,
            sleep: tokio_sleep(),
        }
    }
}

/// Range 分段下载器：`FileDownloader` 的默认实现。
pub struct RangeDownloader {
    client: Arc<dyn SegmentDownloader>,
    config: RangeDownloaderConfig,
    policy: TransferPolicy,
}

/// 探测到的资源形态。
#[derive(Debug, Clone)]
struct ProbedResource {
    total: u64,
    identity: ResourceIdentity,
    /// 跟随重定向后的**最终响应地址**：资源域绑它，不绑入口 URL
    /// （同一个入口两次重定向到不同目标时，只有最终地址能区分资源）。
    effective_url: Option<String>,
}

impl ProbedResource {
    /// 资源域摘要：优先用最终响应地址，实现方没提供时才退回入口 URL。
    fn resource_domain(&self, entry_url: &str) -> String {
        resource_domain(self.effective_url.as_deref().unwrap_or(entry_url))
    }
}

/// 传输计数：有效字节 / 恢复字节 / 本次网络字节分开记。
#[derive(Debug, Default)]
struct TransferCounters {
    /// 已标记完成的字节（单调不减）。
    committed: AtomicU64,
    /// 正在写的字节（重试时扣回，避免重复计入）。
    inflight: AtomicU64,
    /// 本次真实网络字节（含重试浪费的字节）。
    network: AtomicU64,
    /// 断点恢复的字节。
    resumed: AtomicU64,
    /// HTTP 请求次数（含 HEAD / 探测 / 重试）。
    attempts: AtomicU64,
}

impl TransferCounters {
    /// 已验证有效字节：已完成 + 正在写的；**绝不假涨超 total**。
    fn validated(&self, total: u64) -> u64 {
        let value = self.committed.load(Ordering::Relaxed) + self.inflight.load(Ordering::Relaxed);
        value.min(total)
    }

    /// 回退整档单流前清零进度类计数（有效/在飞/恢复），但**保留 `network`**：
    /// 真实网络字节必须包含此前分段与重试已经消耗的流量，网速/成本不能凭空缩水。
    fn reset_progress(&self) {
        self.committed.store(0, Ordering::Relaxed);
        self.inflight.store(0, Ordering::Relaxed);
        self.resumed.store(0, Ordering::Relaxed);
    }

    fn report(&self, total: Option<u64>) -> TransferReport {
        TransferReport {
            network_bytes: self.network.load(Ordering::Relaxed),
            resumed_bytes: self.resumed.load(Ordering::Relaxed),
            validated_bytes: self.committed.load(Ordering::Relaxed),
            attempts: self.attempts.load(Ordering::Relaxed) as u32,
            total_bytes: total,
        }
    }
}

/// 分段路径的失败分类：只有「资源域持续不稳定」允许再降级成整档单流。
///
/// 为什么单独区分：真实 CDN 会把同一个入口的每次请求分到不同边缘（域不同、内容一致），
/// 逐 206 域校验会把这种正常链路判成「资源变化」。一次重启预算仍失配时，
/// 用单流整体重下（单个响应不可能拼片）比直接判死更安全也更符合用户预期。
enum SegmentFailure {
    /// 资源域失配（`RestartReason::OriginMismatch`）。
    DomainUnstable(DownloadError),
    /// 资源变化（416 / 验证器失配）：重新探测后重下。
    ResourceChanged(DownloadError),
    /// 其它失败：原样上报，不做降级。
    Failed(DownloadError),
}

/// 单段任务的共享上下文。
struct SegmentJob<'a> {
    url: &'a str,
    headers: HashMap<String, String>,
    part: &'a Path,
    dest: &'a Path,
    ledger: &'a ControlLedger,
    expected: &'a ResourceIdentity,
    /// 本次任务探测绑定的资源域（最终响应地址摘要）；每条 206 都要与它一致。
    expected_domain: &'a str,
    counters: &'a TransferCounters,
    budget: &'a Arc<ConnectionBudget>,
    track: &'a TransferTrack,
    progress: Option<&'a dyn ProgressSink>,
    total: u64,
}

impl RangeDownloader {
    pub fn new(client: Arc<dyn SegmentDownloader>, config: RangeDownloaderConfig) -> Self {
        Self {
            client,
            config,
            policy: TransferPolicy::default(),
        }
    }

    /// 带传输策略的构造（additive）：定制连接预算 / 退避 / 落盘屏障。
    pub fn with_policy(
        client: Arc<dyn SegmentDownloader>,
        config: RangeDownloaderConfig,
        policy: TransferPolicy,
    ) -> Self {
        Self {
            client,
            config,
            policy,
        }
    }

    /// 报一次进度事件（阶段迁移 + 字节进度共用一个出口）。
    fn emit(
        &self,
        track: &TransferTrack,
        counters: &TransferCounters,
        total: u64,
        phase: TransferPhase,
        progress: Option<&dyn ProgressSink>,
    ) {
        let Some(sink) = progress else { return };
        sink.on_event(&TransferEvent {
            task_id: track.task_id.clone(),
            asset_id: track.asset_id.clone(),
            // 首个探测事件发生在第一次尝试计数之前；对外口径「从 1 起」。
            attempt_id: (counters.attempts.load(Ordering::Relaxed) as u32).max(1),
            phase,
            validated_bytes: counters.validated(total),
            resumed_bytes: counters.resumed.load(Ordering::Relaxed),
            network_bytes: counters.network.load(Ordering::Relaxed),
            total_bytes: Some(total).filter(|value| *value > 0),
        });
    }

    /// HEAD 带重试：只兜网络层抖动，重试耗尽返回 Err（由调用方走范围探测）。
    async fn probe_head_with_retry(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<ProbeOutcome, TransportError> {
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            match self.client.probe(url, headers).await {
                Ok(info) => return Ok(info),
                Err(error) => {
                    if attempt >= self.config.retry_attempts {
                        return Err(error);
                    }
                    (self.config.sleep)(self.config.retry_wait).await;
                }
            }
        }
    }

    /// 范围探测：`GET Range: bytes=0-0` 从 Content-Range 的 total 拿资源长度
    /// （HEAD 被拒/无长度时的兜底；GET 响应同时带回真实验证器与最终响应地址）。
    async fn probe_with_range(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Option<ProbedResource> {
        let probe = self.client.probe_range(url, headers).await?;
        Some(ProbedResource {
            total: probe.total,
            identity: probe.identity,
            effective_url: probe.effective_url,
        })
    }

    /// 资源探测：HEAD 优先，失败/无长度再范围探测，都没有就返回 None（走单流）。
    ///
    /// 整条回退链有界：各试一次，不在 HEAD / 探测 / 单流之间来回打转。
    async fn probe_resource(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Option<ProbedResource> {
        let head = self.probe_head_with_retry(url, headers).await.ok();
        if let Some(probed) = &head {
            let head = &probed.head;
            if (200..300).contains(&head.status) {
                if let Some(total) = head.content_length.filter(|value| *value > 0) {
                    let mut identity = head.identity.clone();
                    if identity.length == 0 {
                        identity.length = total;
                    }
                    return Some(ProbedResource {
                        total,
                        identity,
                        effective_url: probed.effective_url.clone(),
                    });
                }
            }
        }
        // 有界回退：HEAD 只在「拿不到长度」或「方法被拒/服务端出错」时
        // 才做范围探测；HEAD 已经给了确定性拒绝（403/404/410）就不再多发一次 GET。
        let definitive = matches!(
            head.as_ref().map(|probed| probed.head.status),
            Some(403) | Some(404) | Some(410)
        );
        if definitive {
            return None;
        }
        self.probe_with_range(url, headers).await
    }

    /// 分段路径：台账判定 → 补缺失区间（并发=connections）→ 逐项校验 → 原子 rename。
    async fn download_segmented(
        &self,
        job: &SegmentJob<'_>,
        total: u64,
        segments: Vec<SegmentRange>,
    ) -> Result<TransferReport, SegmentFailure> {
        // 待取子区间：每个规划段减去台账已完成的字节（connections 变化也能对上）。
        let mut pending: Vec<ByteRange> = Vec::new();
        for segment in &segments {
            let target = ByteRange::new(segment.start, segment.end);
            pending.extend(job.ledger.pending_of(target));
        }

        let connections = normalize_connections(Some(self.config.connections));
        let outcome: Result<(), SegmentTaskError> = futures::stream::iter(pending)
            .map(|range| {
                let job = SegmentJob {
                    url: job.url,
                    headers: job.headers.clone(),
                    part: job.part,
                    dest: job.dest,
                    ledger: job.ledger,
                    expected: job.expected,
                    expected_domain: job.expected_domain,
                    counters: job.counters,
                    budget: job.budget,
                    track: job.track,
                    progress: job.progress,
                    total: job.total,
                };
                async move { self.download_range_with_retry(range, &job).await }
            })
            .buffer_unordered(connections)
            .try_collect::<Vec<()>>()
            .await
            .map(|_| ());

        match outcome {
            Ok(()) => {}
            Err(SegmentTaskError::RangeIgnored) => {
                // 服务器对 Range 回了 200 全量：不支持分段。清场后整体回退单流。
                discard_partial(job.dest);
                return self
                    .download_stream(job, total)
                    .await
                    .map(|bytes| self.stream_report(job, total, bytes))
                    .map_err(SegmentFailure::Failed);
            }
            Err(SegmentTaskError::Restart(reason)) => {
                // 资源变化 / 416：交给外层有限重启（最多 MAX_RESOURCE_RESTARTS 次）；
                // 资源域失配再多给一次「整档单流」的安全降级。
                keep_partial(job.dest);
                let error =
                    DownloadError::ResourceChanged(format!("需要重新下载（{}）", reason.detail()));
                return Err(match reason {
                    RestartReason::OriginMismatch => SegmentFailure::DomainUnstable(error),
                    _ => SegmentFailure::ResourceChanged(error),
                });
            }
            Err(SegmentTaskError::Final(error)) => {
                // 瞬时故障/永久 fs 错都保留可信已完成段，下次续传。
                keep_partial(job.dest);
                return Err(SegmentFailure::Failed(error));
            }
        }

        // 全段落齐：校验大小再原子交付（rename 在同目录内，任意时刻 dest 都是完整文件）。
        let size = std::fs::metadata(job.part)
            .map_err(|error| {
                keep_partial(job.dest);
                map_fs_error(error)
            })
            .map_err(SegmentFailure::Failed)?
            .len();
        if size != total {
            keep_partial(job.dest);
            return Err(SegmentFailure::Failed(DownloadError::Protocol(format!(
                "下载不完整：预期 {total} 字节，实际落盘 {size} 字节"
            ))));
        }
        if let Err(error) = std::fs::rename(job.part, job.dest) {
            keep_partial(job.dest);
            return Err(SegmentFailure::Failed(map_fs_error(error)));
        }
        // 交付后清控制文件与锁文件；万一这一步被打断，下次无 .part 会自动从零，自愈。
        discard_partial(job.dest);
        Ok(job.counters.report(Some(total)))
    }

    fn stream_report(&self, job: &SegmentJob<'_>, total: u64, bytes: u64) -> TransferReport {
        job.counters.committed.store(bytes, Ordering::Relaxed);
        TransferReport {
            network_bytes: job.counters.network.load(Ordering::Relaxed),
            resumed_bytes: 0,
            validated_bytes: bytes,
            attempts: job.counters.attempts.load(Ordering::Relaxed) as u32,
            total_bytes: Some(total).filter(|value| *value > 0),
        }
    }

    /// 单次区间拉取的重试外壳：瞬时故障按 attempts 重试，fs 永久错直接终止。
    async fn download_range_with_retry(
        &self,
        range: ByteRange,
        job: &SegmentJob<'_>,
    ) -> Result<(), SegmentTaskError> {
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            let mut retry_after = None;
            match self
                .fetch_range_to_offset(range, job, &mut retry_after)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if matches!(error, AttemptError::RangeIgnored) {
                        return Err(SegmentTaskError::RangeIgnored);
                    }
                    if let AttemptError::Restart(reason) = error {
                        return Err(SegmentTaskError::Restart(reason));
                    }
                    if !error.is_retryable() || attempt >= self.config.retry_attempts {
                        return Err(SegmentTaskError::Final(error.into_final()));
                    }
                    let delay = retry_delay(&self.policy.backoff, attempt, retry_after);
                    (self.config.sleep)(delay).await;
                }
            }
        }
    }

    /// 单次区间拉取：Range GET → 206 逐项校验 → 按偏移写 .part → 落盘屏障 → 标记完成。
    async fn fetch_range_to_offset(
        &self,
        range: ByteRange,
        job: &SegmentJob<'_>,
        retry_after: &mut Option<Duration>,
    ) -> Result<(), AttemptError> {
        let _budget: BudgetGuard = job.budget.acquire(&host_of(job.url)).await;
        job.counters.attempts.fetch_add(1, Ordering::Relaxed);
        let segment = SegmentRange {
            index: 0,
            start: range.start,
            end: range.end,
        };
        let scoped = self
            .client
            .get_scoped(job.url, Some(&segment), &job.headers)
            .await
            .map_err(AttemptError::Network)?;
        // 逐响应校验资源域：探测落到 A、这条 Range GET 却跳到 B（A/B 同 size 同 ETag
        // 但内容不同）时，绝不能把 B 的字节写进 A 的产物。按「资源变化」走既有的一次
        // 重启预算：重启后再失配就失败并保留安全断点，不无限重启。
        if let Some(effective) = &scoped.effective_url {
            if resource_domain(effective) != job.expected_domain {
                return Err(AttemptError::Restart(RestartReason::OriginMismatch));
            }
        }
        let response = scoped.response;
        *retry_after = response.retry_after;
        match response.status {
            206 => {}
            // 对带 Range 的请求回 200 全量：服务器不支持分段，上层整体回退单流。
            200 => return Err(AttemptError::RangeIgnored),
            // 区间不可满足：重新探测并有限重启。
            416 => return Err(AttemptError::Restart(RestartReason::TotalMismatch)),
            status => return Err(AttemptError::Status(status)),
        }

        // 206 必须逐项校验 Content-Range：错误区间同长也要拒收。
        match response.content_range {
            Some(ContentRange::Satisfied { start, end, total }) => {
                if start != range.start || end != range.end {
                    return Err(AttemptError::ContentRangeMismatch {
                        expected: range,
                        got: (start, end),
                    });
                }
                if let Some(total) = total {
                    if total != job.total {
                        return Err(AttemptError::Restart(RestartReason::ResourceChanged));
                    }
                }
            }
            Some(ContentRange::Unsatisfied { .. }) => {
                return Err(AttemptError::Restart(RestartReason::TotalMismatch));
            }
            None => return Err(AttemptError::MissingContentRange),
        }
        // 对象验证器比对：换了资源就绝不能把新旧字节拼在一起。
        if response.identity.has_usable_validator()
            && !response.identity.confirms_same_validators(job.expected)
        {
            return Err(AttemptError::Restart(RestartReason::ResourceChanged));
        }

        let mut file = OpenOptions::new()
            .write(true)
            .open(job.part)
            .map_err(|error| AttemptError::Fs(map_fs_error(error)))?;
        file.seek(SeekFrom::Start(range.start))
            .map_err(|error| AttemptError::Fs(map_fs_error(error)))?;

        let mut remaining = range.len();
        let mut received = 0u64;
        let mut written = 0u64;
        let mut stream = response.body;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(AttemptError::Network)?;
            if chunk.len() as u64 > remaining {
                return Err(AttemptError::OverlongSegment);
            }
            file.write_all(&chunk)
                .map_err(|error| AttemptError::Fs(map_fs_error(error)))?;
            let bytes = chunk.len() as u64;
            remaining -= bytes;
            received += bytes;
            written += bytes;
            job.counters.network.fetch_add(bytes, Ordering::Relaxed);
            job.counters.inflight.fetch_add(bytes, Ordering::Relaxed);
            if let Some(sink) = job.progress {
                sink.on_event(&TransferEvent {
                    task_id: job.track.task_id.clone(),
                    asset_id: job.track.asset_id.clone(),
                    attempt_id: job.counters.attempts.load(Ordering::Relaxed) as u32,
                    phase: TransferPhase::Downloading,
                    validated_bytes: job.counters.validated(job.total),
                    resumed_bytes: job.counters.resumed.load(Ordering::Relaxed),
                    network_bytes: job.counters.network.load(Ordering::Relaxed),
                    total_bytes: Some(job.total).filter(|value| *value > 0),
                });
            }
        }
        if remaining > 0 {
            // 短段：扣回本次已写的字节，重试时不会把半截字节算进有效进度。
            job.counters.inflight.fetch_sub(written, Ordering::Relaxed);
            return Err(AttemptError::ShortSegment {
                expected: range.len(),
                received,
            });
        }

        // 不变量：先保证数据落盘，再标记完成（崩溃后绝不把未落盘的段当已完成）。
        self.policy
            .durability
            .sync_part(&file)
            .map_err(|error| AttemptError::Fs(map_fs_error(error)))?;
        job.ledger
            .mark_persisted(range)
            .map_err(|error| AttemptError::Fs(map_fs_error(error)))?;
        job.counters.inflight.fetch_sub(written, Ordering::Relaxed);
        job.counters.committed.fetch_add(written, Ordering::Relaxed);
        Ok(())
    }

    /// 单流路径：不带 Range 的整档 GET → .part → rename（HEAD 拿不到长度/小文件/分段回退）。
    ///
    /// **单流无 Range 只能重下**：不写台账、不产生可续传状态；`.part` 只是原子交付的
    /// 中间名，失败即删，绝不留下没有台账的残件误导下次续传。
    async fn download_stream(
        &self,
        job: &SegmentJob<'_>,
        total: u64,
    ) -> Result<u64, DownloadError> {
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            let _ = std::fs::remove_file(job.part);
            let mut retry_after = None;
            let mut count = 0u64;
            match self
                .stream_once(job, total, &mut count, &mut retry_after)
                .await
            {
                Ok(()) => {
                    if count == 0 {
                        // 空响应是可重试故障。
                        if attempt >= self.config.retry_attempts {
                            discard_partial(job.dest);
                            return Err(DownloadError::Protocol(
                                "下载响应为空，未生成有效文件".into(),
                            ));
                        }
                        (self.config.sleep)(self.config.retry_wait).await;
                        continue;
                    }
                    if let Err(error) = std::fs::rename(job.part, job.dest) {
                        discard_partial(job.dest);
                        return Err(map_fs_error(error));
                    }
                    return Ok(count);
                }
                Err(error) => {
                    if !error.is_retryable() || attempt >= self.config.retry_attempts {
                        // 单流失败清残件：不产生可信部分段，留残件只会误导下次续传。
                        discard_partial(job.dest);
                        return Err(error.into_final());
                    }
                    let delay = retry_delay(&self.policy.backoff, attempt, retry_after);
                    (self.config.sleep)(delay).await;
                }
            }
        }
    }

    async fn stream_once(
        &self,
        job: &SegmentJob<'_>,
        total: u64,
        count: &mut u64,
        retry_after: &mut Option<Duration>,
    ) -> Result<(), AttemptError> {
        let _budget: BudgetGuard = job.budget.acquire(&host_of(job.url)).await;
        job.counters.attempts.fetch_add(1, Ordering::Relaxed);
        let response = self
            .client
            .get(job.url, None, &job.headers)
            .await
            .map_err(AttemptError::Network)?;
        *retry_after = response.retry_after;
        if !(200..300).contains(&response.status) {
            return Err(AttemptError::Status(response.status));
        }
        if response.identity.has_usable_validator()
            && !response.identity.confirms_same_validators(job.expected)
        {
            return Err(AttemptError::Restart(RestartReason::ResourceChanged));
        }
        let mut file =
            File::create(job.part).map_err(|error| AttemptError::Fs(map_fs_error(error)))?;
        let mut stream = response.body;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(AttemptError::Network)?;
            file.write_all(&chunk)
                .map_err(|error| AttemptError::Fs(map_fs_error(error)))?;
            let bytes = chunk.len() as u64;
            *count += bytes;
            job.counters.network.fetch_add(bytes, Ordering::Relaxed);
            job.counters.committed.store(*count, Ordering::Relaxed);
            if let Some(sink) = job.progress {
                sink.on_event(&TransferEvent {
                    task_id: job.track.task_id.clone(),
                    asset_id: job.track.asset_id.clone(),
                    attempt_id: job.counters.attempts.load(Ordering::Relaxed) as u32,
                    phase: TransferPhase::Downloading,
                    validated_bytes: (*count).min(total),
                    resumed_bytes: 0,
                    network_bytes: job.counters.network.load(Ordering::Relaxed),
                    total_bytes: Some(total).filter(|value| *value > 0),
                });
            }
        }
        drop(file);
        Ok(())
    }

    /// 完整下载入口：探测 → 续传判定 → 分段/单流 →（资源变化时有限重启）。
    async fn download_tracked_inner(
        &self,
        track: &TransferTrack,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        if url.trim().is_empty() {
            return Err(DownloadError::Protocol("缺少有效的下载 URL".into()));
        }
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(map_fs_error)?;
            }
        }

        // 每个目标路径只允许一个写入者：不同任务/进程不会互相覆盖同一目标。
        let _lock = DestLock::acquire(dest)
            .map_err(|error| DownloadError::Busy(format!("另一个任务正在写入: {error}")))?;

        let counters = TransferCounters::default();
        let mut restarts = 0u32;
        self.emit(track, &counters, 0, TransferPhase::Probing, progress);
        loop {
            counters.attempts.fetch_add(1, Ordering::Relaxed);
            let probed = self.probe_resource(url, headers).await;
            let (total, identity, origin) = match probed {
                // 资源域绑**探测到的最终响应地址**：同一个入口 URL 两次重定向到不同目标时，
                // 只有最终地址能区分是不是同一份资源（RFC 9110 §7.6 有效请求 URI +
                // §8.8.1 强 validator 不保证跨资源唯一，同 size+同 etag 也会撞）。
                Some(resource) => {
                    let domain = resource.resource_domain(url);
                    (resource.total, resource.identity, domain)
                }
                // 探测不到长度：走单流（HEAD 被拒/无长度的有界回退，）。
                None => {
                    // 探测不到长度 → 单流：不产生可续传台账，资源域按入口 URL 记录即可。
                    let ledger = ControlLedger::open(dest, 0, ResourceIdentity::default(), url);
                    let job = SegmentJob {
                        url,
                        headers: headers.clone(),
                        part: &part_path_for(dest),
                        dest,
                        ledger: &ledger,
                        expected: &ResourceIdentity::default(),
                        expected_domain: &resource_domain(url),
                        counters: &counters,
                        budget: &self.policy.budget,
                        track,
                        progress,
                        total: 0,
                    };
                    let bytes = self.download_stream(&job, 0).await?;
                    let report = self.stream_report(&job, 0, bytes);
                    self.emit(track, &counters, 0, TransferPhase::Done, progress);
                    return Ok(report);
                }
            };

            let decision = decide_resume(dest, &identity, &origin);
            let resuming = matches!(decision, ResumeDecision::Resume(_));
            let ledger = match decision {
                ResumeDecision::Resume(state) => {
                    counters.resumed.store(
                        state.done.iter().map(|range| range.len()).sum(),
                        Ordering::Relaxed,
                    );
                    ControlLedger::resume(dest, *state)
                }
                ResumeDecision::Restart { .. } => {
                    // 不可信现场一律从零：删残件新建（绝不把旧字节拼进新文件）。
                    discard_partial(dest);
                    File::create(part_path_for(dest)).map_err(map_fs_error)?;
                    ControlLedger::open(dest, total, identity.clone(), &origin)
                }
            };

            let segments =
                plan_segments(total, normalize_connections(Some(self.config.connections)));
            // 单段（小文件）不值得开多连接：直接走单流路径。
            // 但有可信断点时仍走分段路径（按区间补缺失字节），否则续传无从谈起。
            let use_segmented = segments.len() > 1 || resuming;

            let job = SegmentJob {
                url,
                headers: self.headers_with_if_range(headers, resuming, &identity),
                part: &part_path_for(dest),
                dest,
                ledger: &ledger,
                expected: &identity,
                expected_domain: &origin,
                counters: &counters,
                budget: &self.policy.budget,
                track,
                progress,
                total,
            };

            let outcome = if use_segmented {
                self.download_segmented(&job, total, segments).await
            } else {
                // 小文件/单段：无 Range 续传语义，从零单流。
                discard_partial(dest);
                let bytes = self.download_stream(&job, total).await?;
                Ok(self.stream_report(&job, total, bytes))
            };

            match outcome {
                Ok(report) => {
                    self.emit(track, &counters, total, TransferPhase::Done, progress);
                    return Ok(report);
                }
                Err(SegmentFailure::DomainUnstable(reason)) => {
                    // 资源域失配：先走一次重启预算（重新探测可能就稳定了）。
                    if restarts < MAX_RESOURCE_RESTARTS {
                        restarts += 1;
                        discard_partial(dest);
                        continue;
                    }
                    // 仍持续失配（真实 CDN 把每条请求分到不同边缘）：安全整体回退单流。
                    // 单个完整响应不可能拼片；从零重建 part，绝不复用任何旧段。
                    let fallback = self
                        .download_stream_from_scratch(&job, total, track, progress)
                        .await;
                    match fallback {
                        Ok(report) => return Ok(report),
                        Err(error) => {
                            self.emit(track, &counters, total, TransferPhase::Failed, progress);
                            // 保留 Http/fs/协议语义（父代理按类别判重试），只给可细分错误补原因。
                            let suffix = format!(
                                "（此前分段下载资源域持续不一致，已回退单流；原因: {reason}）"
                            );
                            return Err(match error {
                                DownloadError::Failed(message) => {
                                    DownloadError::Failed(format!("{message}{suffix}"))
                                }
                                DownloadError::Network(message) => {
                                    DownloadError::Network(format!("{message}{suffix}"))
                                }
                                DownloadError::Protocol(message) => {
                                    DownloadError::Protocol(format!("{message}{suffix}"))
                                }
                                DownloadError::ResourceChanged(message) => {
                                    DownloadError::ResourceChanged(format!("{message}{suffix}"))
                                }
                                other => other,
                            });
                        }
                    }
                }
                Err(SegmentFailure::ResourceChanged(error)) => {
                    // 资源变化（416 / 验证器失配）：重新探测后有限重启。
                    if restarts < MAX_RESOURCE_RESTARTS {
                        restarts += 1;
                        discard_partial(dest);
                        continue;
                    }
                    self.emit(track, &counters, total, TransferPhase::Failed, progress);
                    return Err(error);
                }
                Err(SegmentFailure::Failed(error)) => {
                    self.emit(track, &counters, total, TransferPhase::Failed, progress);
                    return Err(error);
                }
            }
        }
    }

    /// 安全整体回退：从零重建 `.part`，用**单个完整 HTTP 响应**把整档下到底。
    ///
    /// 只在资源域持续失配（一次重启预算用完）时调用：
    /// - 先丢干净分段残件与台账，绝不复用任何旧段、绝不产生跨链路拼接；
    /// - 进度类计数清零（有效/在飞/恢复），**真实网络字节保留**——此前分段与重试的
    ///   流量必须如实计入，网速/成本不能凭空缩水；
    /// - 单流自身的重试与失败清理沿用 `download_stream`，仍然有界，不会无限重启。
    async fn download_stream_from_scratch(
        &self,
        job: &SegmentJob<'_>,
        total: u64,
        track: &TransferTrack,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        discard_partial(job.dest);
        job.counters.reset_progress();
        self.emit(track, job.counters, total, TransferPhase::Probing, progress);
        let bytes = self.download_stream(job, total).await?;
        let report = self.stream_report(job, total, bytes);
        self.emit(track, job.counters, total, TransferPhase::Done, progress);
        Ok(report)
    }

    /// 续传时给 Range 请求补 `If-Range`（强验证器），让服务器兜底 TOCTOU。
    ///
    /// 弱 ETag 不做 `If-Range`（RFC 7232 §3.2）；没有强验证器就不加这个头。
    fn headers_with_if_range(
        &self,
        headers: &HashMap<String, String>,
        resuming: bool,
        identity: &ResourceIdentity,
    ) -> HashMap<String, String> {
        let mut merged = headers.clone();
        if resuming {
            if let Some(value) = identity.if_range_value() {
                merged.insert("If-Range".to_string(), value.to_string());
            }
        }
        merged
    }
}

#[async_trait]
impl FileDownloader for RangeDownloader {
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
    ) -> Result<u64, DownloadError> {
        self.download_with_progress(url, dest, headers, None).await
    }

    /// 完整下载入口（进度可选）：返回**本次真实传输的字节数**（续传时不含已完成段），
    /// 速度看板按它算网速；产物大小由调用方读元数据。
    async fn download_with_progress(
        &self,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<u64, DownloadError> {
        let track = TransferTrack::new("single", "video");
        let report = self
            .download_tracked(&track, url, dest, headers, None, progress)
            .await?;
        Ok(report.network_bytes)
    }

    async fn download_tracked(
        &self,
        track: &TransferTrack,
        url: &str,
        dest: &Path,
        headers: &HashMap<String, String>,
        _page_url: Option<&str>,
        progress: Option<&dyn ProgressSink>,
    ) -> Result<TransferReport, DownloadError> {
        self.download_tracked_inner(track, url, dest, headers, progress)
            .await
    }
}

/// 从 URL 取 host（连接预算按 host 分层）；取不到就退成整串，不阻塞下载。
pub fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(|host| host.to_ascii_lowercase()))
        .unwrap_or_else(|| url.to_string())
}

/// 网络层错误 → 结构化 `DownloadError`（超时单独成类，便于调用方区分）。
fn download_error_from_transport(error: TransportError) -> DownloadError {
    match error {
        TransportError::Timeout => DownloadError::Timeout,
        TransportError::Network(message) => DownloadError::Network(message),
    }
}

/// io::Error → DownloadError 的粗分类（按 `io::ErrorKind` 映射到失败类型）：
/// 磁盘满/权限/路径类永久错误绝不重试。
pub fn map_fs_error(error: std::io::Error) -> DownloadError {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::StorageFull => DownloadError::DiskFull,
        ErrorKind::PermissionDenied => DownloadError::Permission(error.to_string()),
        ErrorKind::NotFound
        | ErrorKind::NotADirectory
        | ErrorKind::IsADirectory
        | ErrorKind::InvalidFilename
        | ErrorKind::DirectoryNotEmpty => DownloadError::Path(error.to_string()),
        _ => DownloadError::Io(error.to_string()),
    }
}

/// 单次尝试的失败分类：重试决策的唯一依据。
#[derive(Debug)]
enum AttemptError {
    Network(TransportError),
    /// HTTP 有响应但状态不可用；可重试性看状态码（403/408/425/429/5xx）。
    Status(u16),
    /// 对带 Range 的请求回了 200 全量——不支持分段，整体回退单流（不重试）。
    RangeIgnored,
    /// 资源变化 / 区间不可满足：交给外层有限重启。
    Restart(RestartReason),
    /// 206 缺 `Content-Range`：无法确认收到的是不是请求的区间。
    MissingContentRange,
    /// `Content-Range` 与请求区间不一致（错误区间同长也必须拒收）。
    ContentRangeMismatch {
        expected: ByteRange,
        got: (u64, u64),
    },
    ShortSegment {
        expected: u64,
        received: u64,
    },
    OverlongSegment,
    Fs(DownloadError),
}

impl AttemptError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Network(_)
            | Self::ShortSegment { .. }
            | Self::ContentRangeMismatch { .. }
            | Self::MissingContentRange => true,
            Self::Status(status) => DownloadFailureKind::is_retryable_http_status(*status),
            Self::RangeIgnored | Self::OverlongSegment | Self::Fs(_) | Self::Restart(_) => false,
        }
    }

    fn into_final(self) -> DownloadError {
        match self {
            Self::Fs(error) => error,
            Self::Network(error) => download_error_from_transport(error),
            Self::Status(status) => DownloadError::Http {
                status,
                detail: "下载服务拒绝".into(),
            },
            Self::ShortSegment { expected, received } => DownloadError::Protocol(format!(
                "段数据不完整（预期 {expected} 字节，收到 {received} 字节）"
            )),
            Self::OverlongSegment => DownloadError::Protocol("段返回数据超过预期长度".into()),
            Self::RangeIgnored => DownloadError::Protocol("服务器不支持分段下载".into()),
            Self::MissingContentRange => {
                DownloadError::Protocol("分段响应缺少 Content-Range，无法确认区间".into())
            }
            Self::ContentRangeMismatch { expected, got } => DownloadError::Protocol(format!(
                "分段区间不符（请求 {}-{}，收到 {}-{}）",
                expected.start, expected.end, got.0, got.1
            )),
            Self::Restart(reason) => {
                DownloadError::ResourceChanged(format!("需要重新下载（{}）", reason.detail()))
            }
        }
    }
}

/// 单段任务内部错误：决定「重试 / 回退单流 / 有限重启 / 终止」。
#[derive(Debug)]
enum SegmentTaskError {
    /// 对带 Range 的请求回了 200 全量——服务器不支持分段，整体回退单流。
    RangeIgnored,
    /// 资源变化 / 416：交给外层有限重启。
    Restart(RestartReason),
    /// 重试耗尽或永久错误的最终形态。
    Final(DownloadError),
}

#[cfg(test)]
mod tests {
    use super::super::control::{control_path_for, ControlState};
    use super::*;
    use crate::transport::plan::MIN_SPLIT_SIZE_BYTES;
    use std::future::Future;
    use std::sync::Mutex;

    const M: u64 = MIN_SPLIT_SIZE_BYTES;

    fn instant_sleep() -> (SleepFn, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&calls);
        (
            Arc::new(move |_duration: Duration| {
                counter.fetch_add(1, Ordering::SeqCst);
                Box::pin(futures::future::ready(()))
            }),
            calls,
        )
    }

    fn config(connections: usize, retry_attempts: usize) -> RangeDownloaderConfig {
        let (sleep, _) = instant_sleep();
        RangeDownloaderConfig {
            connections,
            retry_attempts,
            retry_wait: Duration::from_millis(0),
            sleep,
        }
    }

    fn policy_with(durability: Arc<dyn PartDurability>) -> TransferPolicy {
        TransferPolicy {
            durability,
            ..TransferPolicy::unbounded()
        }
    }

    fn identity(etag: Option<&str>, length: u64) -> ResourceIdentity {
        ResourceIdentity::new(etag.map(|value| value.to_string()), None, length)
    }

    const TEST_URL: &str = "https://cdn.test/v.mp4";

    fn domain_of(url: &str) -> String {
        super::super::identity::resource_domain(url)
    }

    /// 测试假服务器：按脚本回放 HEAD/GET，记录收到的 Range 头。
    #[derive(Default)]
    struct FakeServer {
        body: Mutex<Vec<u8>>,
        head_status: u16,
        head_length: Option<u64>,
        head_etag: Option<String>,
        /// 对带 Range 的 GET 回 200 全量（模拟不支持分段的 CDN）。
        ignore_range: bool,
        /// GET 的固定状态码（非 2xx 时字节流为空）。
        get_status: u16,
        /// 每次 GET 返回的 ETag（模拟签名 URL 刷新后换了资源）。
        get_etag: Option<String>,
        /// 206 回一个与请求区间不同但等长的区间（模拟 CDN 中间层出错）。
        shift_content_range: bool,
        /// 206 不带 Content-Range。
        omit_content_range: bool,
        /// 429/503 时回的 Retry-After 秒数。
        retry_after_secs: Option<u64>,
        /// 这些 Range 头值永远网络失败（模拟某一段反复断网）。
        fail_ranges: Mutex<Vec<String>>,
        /// 前 N 次 GET 直接报网络错误（模拟抖动）。
        fail_first_n_gets: AtomicU64,
        /// 中途断流：段写到一半就掐断。
        truncate_at: Option<u64>,
        /// 模拟「跟随重定向后的最终响应地址」；None = 没有重定向（入口即最终）。
        effective: Mutex<Option<String>>,
        /// 后半段换资源：`range.start >= late_from` 的请求回 `late_body`，
        /// 最终地址报 `late_effective`（模拟逐段跳到另一条 CDN）。
        late_from: Option<u64>,
        late_body: Mutex<Option<Vec<u8>>>,
        late_effective: Mutex<Option<String>>,
        seen_ranges: Mutex<Vec<String>>,
        head_calls: AtomicU64,
        get_calls: AtomicU64,
    }

    impl FakeServer {
        fn set_effective(&self, url: &str) {
            *self.effective.lock().unwrap() = Some(url.to_string());
        }

        fn with_body(body: Vec<u8>) -> Self {
            let len = body.len() as u64;
            Self {
                body: Mutex::new(body),
                head_status: 200,
                head_length: Some(len),
                head_etag: Some(r#""v1""#.into()),
                ..Default::default()
            }
        }

        fn seen_ranges(&self) -> Vec<String> {
            self.seen_ranges.lock().unwrap().clone()
        }

        fn fail_range(&self, header: &str) {
            self.fail_ranges.lock().unwrap().push(header.to_string());
        }

        fn get_calls(&self) -> u64 {
            self.get_calls.load(Ordering::SeqCst)
        }

        fn head_calls(&self) -> u64 {
            self.head_calls.load(Ordering::SeqCst)
        }

        fn chunked(data: Vec<u8>) -> ByteStream {
            let chunks: Vec<Result<Vec<u8>, TransportError>> =
                data.chunks(64 * 1024).map(|c| Ok(c.to_vec())).collect();
            Box::pin(futures::stream::iter(chunks))
        }

        fn truncated(data: Vec<u8>, at: u64) -> ByteStream {
            let keep = data[..(at as usize).min(data.len())].to_vec();
            let mut chunks: Vec<Result<Vec<u8>, TransportError>> =
                keep.chunks(64 * 1024).map(|c| Ok(c.to_vec())).collect();
            chunks.push(Err(TransportError::Network("connection reset".into())));
            Box::pin(futures::stream::iter(chunks))
        }
    }

    #[async_trait]
    impl SegmentDownloader for FakeServer {
        async fn probe(
            &self,
            url: &str,
            headers: &HashMap<String, String>,
        ) -> Result<super::ProbeOutcome, TransportError> {
            Ok(super::ProbeOutcome {
                head: self.probe_head(url, headers).await?,
                effective_url: self.effective.lock().unwrap().clone(),
            })
        }

        /// 与真实 wreq 同口径：0-0 探测也带回本次请求的最终地址。
        async fn probe_range(
            &self,
            url: &str,
            headers: &HashMap<String, String>,
        ) -> Option<super::RangeProbeOutcome> {
            let probe = SegmentRange {
                index: 0,
                start: 0,
                end: 0,
            };
            let response = self.get(url, Some(&probe), headers).await.ok()?;
            if response.status != 206 {
                return None;
            }
            let total = response
                .content_range
                .as_ref()
                .and_then(|range| range.total())?;
            Some(super::RangeProbeOutcome {
                total,
                identity: response.identity,
                effective_url: self.effective.lock().unwrap().clone(),
            })
        }

        async fn probe_head(
            &self,
            _url: &str,
            _headers: &HashMap<String, String>,
        ) -> Result<HeadInfo, TransportError> {
            self.head_calls.fetch_add(1, Ordering::SeqCst);
            Ok(HeadInfo {
                status: self.head_status,
                content_length: self.head_length,
                identity: identity(self.head_etag.as_deref(), self.head_length.unwrap_or(0)),
                accept_ranges: true,
            })
        }

        async fn get(
            &self,
            url: &str,
            range: Option<&SegmentRange>,
            headers: &HashMap<String, String>,
        ) -> Result<SegmentResponse, TransportError> {
            Ok(self.get_scoped(url, range, headers).await?.response)
        }

        async fn get_scoped(
            &self,
            _url: &str,
            range: Option<&SegmentRange>,
            headers: &HashMap<String, String>,
        ) -> Result<super::ScopedResponse, TransportError> {
            let response = self.serve(range, headers).await?;
            let late = matches!(
                (self.late_from, range),
                (Some(from), Some(range)) if range.start >= from
            );
            let effective = if late {
                self.late_effective.lock().unwrap().clone()
            } else {
                self.effective.lock().unwrap().clone()
            };
            Ok(super::ScopedResponse {
                response,
                effective_url: effective,
            })
        }
    }

    impl FakeServer {
        async fn serve(
            &self,
            range: Option<&SegmentRange>,
            headers: &HashMap<String, String>,
        ) -> Result<SegmentResponse, TransportError> {
            let _ = headers;
            self.get_calls.fetch_add(1, Ordering::SeqCst);
            self.seen_ranges.lock().unwrap().push(
                range
                    .map(|r| r.header_value())
                    .unwrap_or_else(|| "none".into()),
            );
            if self.fail_first_n_gets.load(Ordering::SeqCst) > 0 {
                self.fail_first_n_gets.fetch_sub(1, Ordering::SeqCst);
                return Err(TransportError::Network("connection reset".into()));
            }
            if let Some(header) = range.map(|r| r.header_value()) {
                if self
                    .fail_ranges
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|blocked| blocked == &header)
                {
                    return Err(TransportError::Network("connection reset".into()));
                }
            }
            let late = matches!(
                (self.late_from, range),
                (Some(from), Some(range)) if range.start >= from
            );
            let body = if late {
                self.late_body
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| self.body.lock().unwrap().clone())
            } else {
                self.body.lock().unwrap().clone()
            };
            let total = body.len() as u64;
            let etag = self.get_etag.clone().or_else(|| self.head_etag.clone());
            let identity = identity(etag.as_deref(), total);
            let retry_after = self.retry_after_secs.map(Duration::from_secs);
            match range {
                Some(range) if !self.ignore_range && self.get_status == 0 => {
                    let start = range.start as usize;
                    let end = (range.end + 1) as usize;
                    let slice = body[start..end].to_vec();
                    let content_range = if self.omit_content_range {
                        None
                    } else {
                        let (start, end) = if self.shift_content_range {
                            (start as u64 + 1, end as u64)
                        } else {
                            (range.start, range.end)
                        };
                        Some(ContentRange::Satisfied {
                            start,
                            end,
                            total: Some(total),
                        })
                    };
                    let body_stream = match self.truncate_at {
                        Some(at) => Self::truncated(slice, at),
                        None => Self::chunked(slice),
                    };
                    Ok(SegmentResponse {
                        status: 206,
                        content_length: Some(range.len()),
                        body: body_stream,
                        content_range,
                        identity,
                        retry_after,
                    })
                }
                Some(_) if self.ignore_range => Ok(SegmentResponse {
                    status: 200,
                    content_length: Some(total),
                    body: Self::chunked(body),
                    content_range: None,
                    identity,
                    retry_after,
                }),
                _ => {
                    let status = if self.get_status == 0 {
                        200
                    } else {
                        self.get_status
                    };
                    let payload = if (200..300).contains(&status) {
                        body
                    } else {
                        Vec::new()
                    };
                    Ok(SegmentResponse {
                        status,
                        content_length: Some(payload.len() as u64),
                        body: Self::chunked(payload),
                        content_range: if self.omit_content_range {
                            None
                        } else {
                            Some(ContentRange::Satisfied {
                                start: 0,
                                end: total.saturating_sub(1),
                                total: Some(total),
                            })
                        },
                        identity,
                        retry_after,
                    })
                }
            }
        }
    }

    /// 记录进度事件峰值：用于断言「进度不假涨过 total」。
    #[derive(Default)]
    struct RecordingSink {
        max: AtomicU64,
        last: AtomicU64,
    }

    impl RecordingSink {
        fn max_validated(&self) -> u64 {
            self.max.load(Ordering::SeqCst)
        }
    }

    impl ProgressSink for RecordingSink {
        fn on_event(&self, event: &TransferEvent) {
            self.max.fetch_max(event.validated_bytes, Ordering::SeqCst);
            self.last.store(event.validated_bytes, Ordering::SeqCst);
        }
    }

    fn no_headers() -> HashMap<String, String> {
        HashMap::new()
    }

    fn track() -> TransferTrack {
        TransferTrack::new("task-1", "video")
    }

    fn run<F: Future>(future: F) -> F::Output {
        pollster::block_on(future)
    }

    #[test]
    fn 分段下载按偏移落盘并原子交付() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("video.mp4");
        let body: Vec<u8> = (0..(2 * M + M / 2) as u32)
            .map(|i| (i % 251) as u8)
            .collect();
        let server = Arc::new(FakeServer::with_body(body.clone()));

        let report = run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .map(|bytes| TransferReport {
            network_bytes: bytes,
            validated_bytes: bytes,
            attempts: 1,
            resumed_bytes: 0,
            total_bytes: Some(body.len() as u64),
        })
        .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body, "产物与源一致");
        assert_eq!(report.network_bytes, body.len() as u64);
        let mut ranges = server.seen_ranges();
        ranges.sort();
        assert_eq!(
            ranges,
            vec![
                "bytes=0-1310719".to_string(),
                "bytes=1310720-2621439".to_string(),
            ],
            "2.5M 按 1M 下限切 2 段"
        );
        assert_eq!(server.head_calls(), 1, "先 HEAD 预检");
        assert!(!part_path_for(&dest).exists(), "交付后无 .part 残件");
        assert!(!control_path_for(&dest).exists(), "交付后无 .aria2 残件");
    }

    #[test]
    fn 小文件不带range单流下载() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("small.mp4");
        let body = vec![7u8; 500 * 1024];
        let server = Arc::new(FakeServer::with_body(body.clone()));

        run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/s.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(server.seen_ranges(), vec!["none".to_string()]);
    }

    #[test]
    fn head无长度时范围探测仍可分段() {
        // HEAD 不给长度但 GET 支持 Range：用 `bytes=0-0` 探测 total，仍能分段。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(2 * M + M / 2) as u32)
            .map(|i| (i % 253) as u8)
            .collect();
        let server = Arc::new(FakeServer {
            head_length: None,
            ..FakeServer::with_body(body.clone())
        });

        run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let ranges = server.seen_ranges();
        assert!(
            ranges.contains(&"bytes=0-0".to_string()),
            "HEAD 无长度要用范围探测: {ranges:?}"
        );
    }

    #[test]
    fn head被拒仍可下载() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let server = Arc::new(FakeServer {
            head_status: 405,
            head_etag: None,
            ..FakeServer::with_body(vec![1u8; 1024])
        });

        run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        // HEAD 被拒不能挡住下载：GET 才是事实来源。
        // 有界回退：先用 `bytes=0-0` 范围探测取长度，再单流。
        assert!(dest.exists());
        assert!(
            server.seen_ranges().contains(&"bytes=0-0".to_string()),
            "先范围探测取长度: {:?}",
            server.seen_ranges()
        );
    }

    #[test]
    fn 服务器忽略range时回退单流() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body = vec![9u8; (2 * M + M / 2) as usize];
        let server = Arc::new(FakeServer {
            ignore_range: true,
            ..FakeServer::with_body(body.clone())
        });

        run(RangeDownloader::new(server.clone(), config(8, 1)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body, "回退单流拿到完整文件");
        assert!(
            server.seen_ranges().contains(&"none".to_string()),
            "最终必须有不带 Range 的单流请求"
        );
        assert!(!part_path_for(&dest).exists());
    }

    #[test]
    fn 错误区间同长必须拒收() {
        // 请求 bytes=0-99 却收到 100-199（同长）：光比字节长度发现不了，必须比 Content-Range。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 251) as u8).collect();
        let server = Arc::new(FakeServer {
            shift_content_range: true,
            ..FakeServer::with_body(body.clone())
        });

        let error = run(RangeDownloader::new(server.clone(), config(8, 1)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap_err();

        assert!(
            error.to_string().contains("区间不符"),
            "错误区间必须拒收: {error}"
        );
        assert!(!dest.exists(), "不能交付拿错区间的产物");
    }

    #[test]
    fn 缺content_range的206不能当成功() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 251) as u8).collect();
        let server = Arc::new(FakeServer {
            omit_content_range: true,
            ..FakeServer::with_body(body.clone())
        });

        let error = run(RangeDownloader::new(server.clone(), config(8, 1)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap_err();

        assert!(error.to_string().contains("Content-Range"), "{error}");
    }

    #[test]
    fn 下载中途换资源要重新下载而不是拼接() {
        // 签名 URL 刷新后同一地址换了内容：验证器失配必须整档重下，绝不把新旧字节拼一起。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 251) as u8).collect();
        let server = Arc::new(FakeServer {
            get_etag: Some(r#""v2""#.into()),
            ..FakeServer::with_body(body.clone())
        });

        let error = run(RangeDownloader::new(server.clone(), config(8, 1)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap_err();

        assert!(
            error.to_string().contains("资源变化"),
            "验证器失配要判资源变化: {error}"
        );
        assert!(!dest.exists(), "绝不错误交付旧 dest");
    }

    #[test]
    fn 断网重试耗尽保留可信已完成段() {
        // 段 0 正常、段 1 反复断网直到重试耗尽：段 0 必须留着，下次续传不用重下。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(2 * M + M / 2) as u32)
            .map(|i| (i % 249) as u8)
            .collect();
        let server = Arc::new(FakeServer::with_body(body.clone()));
        // 2.5M 切 2 段：段 1 是 bytes=1310720-2621439。
        server.fail_range("bytes=1310720-2621439");

        let error = run(RangeDownloader::new(server.clone(), config(8, 2)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap_err();

        assert!(!dest.exists(), "失败绝不交付 dest");
        assert!(part_path_for(&dest).exists(), "断网要保留半成品");
        assert!(control_path_for(&dest).exists(), "断网要保留断点台账");
        let state = super::super::control::load_control_state(&dest).unwrap();
        assert!(
            state.is_done(ByteRange::new(0, M + M / 4 - 1)),
            "段 0 已完成的字节必须记进台账: {:?}",
            state.done
        );
        assert!(
            !state.is_done(ByteRange::new(1310720, 2621439)),
            "段 1 没下完不能标完成"
        );
        let _ = error;
    }

    #[test]
    fn 跨进程续传只补缺失区间() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(2 * M + M / 2) as u32)
            .map(|i| (i % 249) as u8)
            .collect();
        // 预置「上一次进程」留下的现场：前 1.25M 已完成，后半是垃圾占位。
        let mut partial = body[..(M + M / 4) as usize].to_vec();
        partial.extend(vec![0xEEu8; (M + M / 4) as usize]);
        std::fs::write(part_path_for(&dest), &partial).unwrap();
        let mut state = ControlState::new(
            body.len() as u64,
            identity(Some(r#""v1""#), body.len() as u64),
            Some(domain_of(TEST_URL)),
        );
        state.mark_done(ByteRange::new(0, M + M / 4 - 1));
        super::super::control::save_control(&dest, &state).unwrap();

        let server = Arc::new(FakeServer::with_body(body.clone()));
        let report = run(
            RangeDownloader::new(server.clone(), config(8, 3)).download_tracked(
                &track(),
                "https://cdn.test/v.mp4",
                &dest,
                &no_headers(),
                None,
                None,
            ),
        )
        .unwrap();

        assert_eq!(
            server.seen_ranges(),
            vec!["bytes=1310720-2621439".to_string()],
            "只补缺失区间"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            body,
            "续传段按偏移覆写垃圾占位"
        );
        assert_eq!(
            report.network_bytes,
            M + M / 4,
            "返回值是本次真实传输字节，不含已完成段"
        );
        assert_eq!(report.resumed_bytes, M + M / 4, "恢复字节单独记");
        assert!(!part_path_for(&dest).exists());
    }

    #[test]
    fn 换连接数后按字节区间而不是段下标续传() {
        // 上次 8 段的下标 2 ≠ 这次 2 段的下标 2；台账必须按字节区间对上。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(4 * M) as u32).map(|i| (i % 241) as u8).collect();
        let mut partial = body.clone();
        partial[(2 * M as usize)..].fill(0xEE);
        std::fs::write(part_path_for(&dest), &partial).unwrap();
        let mut state = ControlState::new(
            body.len() as u64,
            identity(Some(r#""v1""#), body.len() as u64),
            Some(domain_of(TEST_URL)),
        );
        // 上次记的是 8 段里前 2 段的字节区间（0..2M）。
        state.mark_done(ByteRange::new(0, 2 * M - 1));
        super::super::control::save_control(&dest, &state).unwrap();

        let server = Arc::new(FakeServer::with_body(body.clone()));
        // 这次只用 2 个连接：段 0 = 0..2M（已完成）、段 1 = 2M..4M（待取）。
        run(RangeDownloader::new(server.clone(), config(2, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        assert_eq!(
            server.seen_ranges(),
            vec!["bytes=2097152-4194303".to_string()],
            "连接数变化后仍只补缺失字节"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), body);
    }

    #[test]
    fn 同大小换内容识别出来并安全重下() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 243) as u8).collect();
        // 现场是旧资源（etag v1）留下的。
        std::fs::write(part_path_for(&dest), vec![0xEEu8; (2 * M) as usize]).unwrap();
        let mut state = ControlState::new(
            body.len() as u64,
            identity(Some(r#""v1""#), body.len() as u64),
            Some(domain_of(TEST_URL)),
        );
        state.mark_done(ByteRange::new(0, M - 1));
        super::super::control::save_control(&dest, &state).unwrap();

        // 服务器现在是 v2（同大小换内容）。
        let server = Arc::new(FakeServer {
            head_etag: Some(r#""v2""#.into()),
            get_etag: Some(r#""v2""#.into()),
            ..FakeServer::with_body(body.clone())
        });
        run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            body,
            "识别到换内容后从零重下，产物只含新内容"
        );
    }

    #[test]
    fn 跨url同size同etag绝不拼旧字节() {
        // RFC 9110 §8.8.1：不同资源的强 validator 不保证全局唯一（各自 "v1" 很常见）。
        // 两个 URL 各自 etag="v1"、同 size、内容不同：切换 URL（换 CDN/换候选）后
        // 绝不复用旧段，产物必须全新资源的内容。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body_a: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 233) as u8).collect();
        let body_b: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 199) as u8).collect();
        // 「上一次进程」用 URL A 下到一半留下的现场：etag "v1"、前 1M 已完成。
        let mut partial = body_a[..M as usize].to_vec();
        partial.extend(vec![0xEEu8; M as usize]);
        std::fs::write(part_path_for(&dest), &partial).unwrap();
        let mut state = ControlState::new(
            body_a.len() as u64,
            identity(Some(r#""v1""#), body_a.len() as u64),
            Some(domain_of("https://cdn-a.test/v.mp4")),
        );
        state.mark_done(ByteRange::new(0, M - 1));
        super::super::control::save_control(&dest, &state).unwrap();

        // 这次换 URL B：同 size、同 etag、内容不同。
        let server = Arc::new(FakeServer::with_body(body_b.clone()));
        run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn-b.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            body_b,
            "切换 URL 后产物必须全是 B，绝不把 A 的旧段拼进去"
        );
        assert!(
            server
                .seen_ranges()
                .contains(&"bytes=0-1048575".to_string()),
            "跨 URL 一律从零重下（第一段也要重新取）: {:?}",
            server.seen_ranges()
        );
    }

    #[test]
    fn 资源域绑最终响应地址而不是入口url() {
        // 入口 URL 完全不变，重定向可能到 A 或 B；A/B 各自 etag="v1"、同 size、内容不同。
        // 资源域必须绑**最终响应地址**（RFC 9110 §7.6 有效请求 URI + §8.8.1）：
        // 同目标要能续传，换目标必须重下，只绑入口两项都做不到。
        let body_a: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 229) as u8).collect();
        let body_b: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 197) as u8).collect();

        // 「上一次」经入口 302 到 A：尾段已落盘（.part 尺寸 = total），台账记 A 的最终地址。
        let seed = |dir: &std::path::Path| {
            let dest = dir.join("v.mp4");
            let mut partial = vec![0xEEu8; M as usize];
            partial.extend(body_a[M as usize..].to_vec());
            std::fs::write(part_path_for(&dest), &partial).unwrap();
            let mut state = ControlState::new(
                body_a.len() as u64,
                identity(Some(r#""v1""#), body_a.len() as u64),
                Some(domain_of("https://cdn-a.test/v.mp4")),
            );
            state.mark_done(ByteRange::new(M, 2 * M - 1));
            super::super::control::save_control(&dest, &state).unwrap();
            dest
        };

        // 正向：入口再次重定向到同一个 A → 只补缺失的前半段（证明资源域真的在看最终地址）。
        let dir_same = tempfile::tempdir().unwrap();
        let dest_same = seed(dir_same.path());
        let server_a = Arc::new(FakeServer::with_body(body_a.clone()));
        server_a.set_effective("https://cdn-a.test/v.mp4");
        run(
            RangeDownloader::new(server_a.clone(), config(8, 1)).download(
                "https://entry.test/v.mp4",
                &dest_same,
                &no_headers(),
            ),
        )
        .unwrap();
        assert_eq!(
            server_a.seen_ranges(),
            vec!["bytes=0-1048575".to_string()],
            "重定向到同一个最终地址时必须续传，不能因为入口 URL 不同就重下"
        );
        assert_eq!(std::fs::read(&dest_same).unwrap(), body_a);

        // 反向：入口不变但重定向改到 B（同 size 同 etag 不同内容）→ 必须从零重下。
        let dir_changed = tempfile::tempdir().unwrap();
        let dest_changed = seed(dir_changed.path());
        let server_b = Arc::new(FakeServer::with_body(body_b.clone()));
        server_b.set_effective("https://cdn-b.test/v.mp4");
        run(
            RangeDownloader::new(server_b.clone(), config(8, 1)).download(
                "https://entry.test/v.mp4",
                &dest_changed,
                &no_headers(),
            ),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(&dest_changed).unwrap(),
            body_b,
            "输出必须全是 B，绝不能把 A 的旧段拼进去"
        );
        assert!(
            server_b
                .seen_ranges()
                .contains(&"bytes=1048576-2097151".to_string()),
            "最终地址变了就必须从零重下（A 已完成的尾段也要重新取）: {:?}",
            server_b.seen_ranges()
        );
    }

    #[test]
    fn 域持续失配时安全回退单流且绝不混片() {
        // 真实 CDN 常见：HEAD/每条 Range 落到不同边缘，域每次都不同（内容其实一致）。
        // 逐 206 域校验仍然保留，但一次重启预算后不能把正常下载判死，
        // 必须安全整体回退单流：从零重建 part、单个完整响应下到底，绝不拼 A/B。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body_a: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 227) as u8).collect();
        let body_b: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 191) as u8).collect();

        // 首段仍回 A（字节真实读入），后段改报边缘 B；不带 Range 的整档 GET 回 A。
        let server = Arc::new(FakeServer {
            late_from: Some(M),
            late_body: Mutex::new(Some(body_b.clone())),
            late_effective: Mutex::new(Some("https://cdn-b.test/v.mp4".into())),
            ..FakeServer::with_body(body_a.clone())
        });
        server.set_effective("https://cdn-a.test/v.mp4");

        let progress = Arc::new(RecordingSink::default());
        let report = run(
            RangeDownloader::new(server.clone(), config(8, 1)).download_tracked(
                &track(),
                "https://entry.test/v.mp4",
                &dest,
                &no_headers(),
                None,
                Some(progress.as_ref()),
            ),
        )
        .expect("域持续不稳定时必须安全回退单流成功，而不是判死");

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            body_a,
            "交付必须是单流那一份完整内容，绝不混入其它链路的字节"
        );
        assert_eq!(report.validated_bytes, 2 * M, "有效字节 = 产物大小");
        assert!(
            server.seen_ranges().contains(&"none".to_string()),
            "必须真的走了整档单流: {:?}",
            server.seen_ranges()
        );
        assert!(
            report.network_bytes > report.validated_bytes,
            "真实网络字节必须包含此前分段/重试的消耗（network={}, validated={}）",
            report.network_bytes,
            report.validated_bytes
        );
        assert!(
            progress.max_validated() <= 2 * M,
            "进度绝不假涨过 total（观测到 {}）",
            progress.max_validated()
        );
        assert_eq!(progress.max_validated(), 2 * M, "最终进度到齐");
        // 首轮 2 段 + 重启 2 段 + 回退 1 次整档 = 5，绝不允许打转。
        assert!(
            server.get_calls() <= 5,
            "只允许一次重启预算 + 一次单流回退（实际 GET 次数 {}）",
            server.get_calls()
        );
    }

    #[test]
    fn 探测器走0到0时资源域也取最终地址() {
        // HEAD 被拒（405）→ 0-0 范围探测；这次探测的最终地址是 B。
        // 台账里记的也是 B 的域 → 必须续传（证明 0-0 分支口径与 HEAD 分支一致）。
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 223) as u8).collect();
        let mut partial = vec![0xEEu8; M as usize];
        partial.extend(body[M as usize..].to_vec());
        std::fs::write(part_path_for(&dest), &partial).unwrap();
        let mut state = ControlState::new(
            body.len() as u64,
            identity(Some(r#""v1""#), body.len() as u64),
            Some(domain_of("https://cdn-b.test/v.mp4")),
        );
        state.mark_done(ByteRange::new(M, 2 * M - 1));
        super::super::control::save_control(&dest, &state).unwrap();

        let server = Arc::new(FakeServer {
            head_status: 405,
            head_length: None,
            ..FakeServer::with_body(body.clone())
        });
        server.set_effective("https://cdn-b.test/v.mp4");

        run(RangeDownloader::new(server.clone(), config(8, 1)).download(
            "https://entry.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body, "0-0 探测后正常续传");
        assert!(
            server
                .seen_ranges()
                .contains(&"bytes=0-1048575".to_string()),
            "只补缺失的前半段: {:?}",
            server.seen_ranges()
        );
        assert!(
            !server
                .seen_ranges()
                .contains(&"bytes=1048576-2097151".to_string()),
            "已完成的尾段不该重取"
        );
    }

    #[test]
    fn 无验证器的台账无法确认资源身份一律重下() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body: Vec<u8> = (0..(2 * M) as u32).map(|i| (i % 239) as u8).collect();
        std::fs::write(part_path_for(&dest), vec![0xEEu8; (2 * M) as usize]).unwrap();
        let mut state = ControlState::new(
            body.len() as u64,
            identity(None, body.len() as u64),
            Some(domain_of(TEST_URL)),
        );
        state.mark_done(ByteRange::new(0, M - 1));
        super::super::control::save_control(&dest, &state).unwrap();

        let server = Arc::new(FakeServer {
            head_etag: None,
            get_etag: None,
            ..FakeServer::with_body(body.clone())
        });
        run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            body,
            "没有验证器就无法确认资源身份，宁可重下"
        );
    }

    #[test]
    fn 禁止直链403重试耗尽后干净失败() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        // 小文件 + HEAD 带长度：只走单流，请求次数可精确断言（不做范围探测）。
        let server = Arc::new(FakeServer {
            get_status: 403,
            head_etag: None,
            ..FakeServer::with_body(vec![0u8; 100 * 1024])
        });

        let error = run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap_err();

        assert!(
            matches!(error, DownloadError::Http { status: 403, .. }),
            "{error:?}"
        );
        assert!(error.is_retryable_http(), "直链 403 是瞬时拒绝");
        assert_eq!(server.get_calls(), 3, "重试耗尽才失败");
        assert!(
            !dest.exists() && !part_path_for(&dest).exists(),
            "失败不留残件"
        );
    }

    #[test]
    fn 状态404是确定性失败不空转() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let server = Arc::new(FakeServer {
            get_status: 404,
            ..FakeServer::with_body(vec![0u8; 100 * 1024])
        });

        let error = run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap_err();

        assert!(
            matches!(error, DownloadError::Http { status: 404, .. }),
            "{error:?}"
        );
        assert!(!error.is_retryable_http());
        assert_eq!(server.get_calls(), 1, "404 是确定性失败，不空转");
    }

    #[test]
    fn retry_after优先于指数退避() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let server = Arc::new(FakeServer {
            get_status: 429,
            retry_after_secs: Some(0),
            ..FakeServer::with_body(vec![0u8; 100 * 1024])
        });

        let error = run(RangeDownloader::new(server.clone(), config(8, 2)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap_err();

        assert!(
            matches!(error, DownloadError::Http { status: 429, .. }),
            "{error:?}"
        );
        assert!(error.is_retryable_http(), "429 可重试");
        assert_eq!(server.get_calls(), 2, "按 Retry-After 重试到耗尽");
    }

    #[test]
    fn 网络抖动重试后成功() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body = vec![5u8; 300 * 1024];
        let server = Arc::new(FakeServer::with_body(body.clone()));
        server.fail_first_n_gets.store(2, Ordering::SeqCst);

        run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(server.get_calls(), 3, "两次抖动后第三次成功");
    }

    #[test]
    fn 空响应是可重试故障并干净失败() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        // HEAD 声称 1 字节但 GET 回空体：按空响应重试到耗尽。
        let server = Arc::new(FakeServer {
            head_length: Some(1),
            ..FakeServer::with_body(Vec::new())
        });

        let error = run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "https://cdn.test/v.mp4",
            &dest,
            &no_headers(),
        ))
        .unwrap_err();

        assert!(
            error.to_string().contains("空"),
            "空响应要给出中文话术: {error}"
        );
        assert_eq!(server.get_calls(), 3);
        assert!(!dest.exists() && !part_path_for(&dest).exists());
    }

    #[test]
    fn 进度重试不假涨且有效字节不超总量() {
        // 段中途断流重试：重试字节不重复计入有效进度，validated 永不超过 total。
        #[derive(Default)]
        struct Recorder {
            events: Mutex<Vec<TransferEvent>>,
        }
        impl ProgressSink for Recorder {
            fn on_event(&self, event: &TransferEvent) {
                self.events.lock().unwrap().push(event.clone());
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body = vec![3u8; (2 * M) as usize];
        let server = Arc::new(FakeServer {
            truncate_at: Some(64 * 1024),
            ..FakeServer::with_body(body.clone())
        });
        let recorder = Arc::new(Recorder::default());

        let sink = Arc::clone(&recorder);
        let _error = run(
            RangeDownloader::new(server.clone(), config(8, 2)).download_tracked(
                &track(),
                "https://cdn.test/v.mp4",
                &dest,
                &no_headers(),
                None,
                Some(sink.as_ref()),
            ),
        )
        .unwrap_err();
        assert!(!dest.exists());

        let events = recorder.events.lock().unwrap().clone();
        assert!(!events.is_empty(), "要有进度事件");
        for event in &events {
            assert!(
                event.validated_bytes <= body.len() as u64,
                "有效字节绝不能超过总量: {:?}",
                event
            );
            assert_eq!(event.task_id, "task-1");
            assert_eq!(event.asset_id, "video");
        }
    }

    #[test]
    fn 多任务进度事件带task_id区分() {
        #[derive(Default)]
        struct Recorder {
            events: Mutex<Vec<TransferEvent>>,
        }
        impl ProgressSink for Recorder {
            fn on_event(&self, event: &TransferEvent) {
                self.events.lock().unwrap().push(event.clone());
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let body = vec![3u8; 100 * 1024];
        let server = Arc::new(FakeServer::with_body(body.clone()));
        let recorder = Arc::new(Recorder::default());

        let sink = Arc::clone(&recorder);
        run(
            RangeDownloader::new(server.clone(), config(8, 3)).download_tracked(
                &TransferTrack::new("task-A", "video"),
                "https://cdn.test/a.mp4",
                &dir.path().join("a.mp4"),
                &no_headers(),
                None,
                Some(sink.as_ref()),
            ),
        )
        .unwrap();

        let events = recorder.events.lock().unwrap().clone();
        assert!(events.iter().all(|event| event.task_id == "task-A"));
        assert!(events.iter().all(|event| event.asset_id == "video"));
    }

    #[test]
    fn 缺url立即失败不发请求() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let server = Arc::new(FakeServer::with_body(vec![1u8; 1024]));
        let error = run(RangeDownloader::new(server.clone(), config(8, 3)).download(
            "",
            &dest,
            &no_headers(),
        ))
        .unwrap_err();
        assert!(error.to_string().contains("URL"));
        assert_eq!(server.get_calls(), 0, "没发请求");
    }

    #[test]
    fn 数据没落盘就不标记完成() {
        // 落盘屏障失败（模拟 fsync 出错）：绝不能把段标成已完成，否则崩溃后会错误交付。
        #[derive(Debug)]
        struct FailingSync;
        impl PartDurability for FailingSync {
            fn sync_part(&self, _file: &File) -> std::io::Result<()> {
                Err(std::io::Error::other("fsync failed"))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("v.mp4");
        let body = vec![3u8; (2 * M) as usize];
        let server = Arc::new(FakeServer::with_body(body.clone()));
        let _error = run(RangeDownloader::with_policy(
            server.clone(),
            config(8, 1),
            policy_with(Arc::new(FailingSync)),
        )
        .download("https://cdn.test/v.mp4", &dest, &no_headers()))
        .unwrap_err();

        assert!(!dest.exists(), "不交付");
        if let Ok(state) = super::super::control::load_control_state(&dest) {
            assert!(
                state.done.is_empty(),
                "数据没落盘就标记完成会让崩溃后错误交付: {:?}",
                state.done
            );
        }
    }

    #[test]
    fn fs错误映射标记永久类别() {
        let disk_full = map_fs_error(std::io::Error::from(std::io::ErrorKind::StorageFull));
        assert!(matches!(disk_full, DownloadError::DiskFull));
        let denied = map_fs_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(matches!(denied, DownloadError::Permission(_)));
        let missing = map_fs_error(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(matches!(missing, DownloadError::Path(_)));
        let other = map_fs_error(std::io::Error::other("boom"));
        assert!(matches!(other, DownloadError::Io(_)));
        for error in [disk_full, denied, missing, other] {
            assert!(error.kind().is_permanent(), "fs 错误一律永久，绝不重试");
        }
    }
}
