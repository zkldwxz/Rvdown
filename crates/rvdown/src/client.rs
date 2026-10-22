//! SDK 门面：`Client` 是 `Engine` 的构建器式入口。
//!
//! ```no_run
//! # async fn run() -> Result<(), rvdown::JobError> {
//! let client = rvdown::Client::builder()
//!     .connections(16)
//!     .job_concurrency(4)
//!     .build()?;
//! let outcome = client.run(&rvdown::Job {
//!     url: "https://cdn.example.com/v.mp4".into(),
//!     out: "/tmp/v.mp4".into(),
//!     ..Default::default()
//! }).await;
//! # let _ = outcome;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::engine::{Engine, JobError, Outcome};
use crate::job::Job;
use crate::net::DnsResolver;
use crate::traits::ProgressSink;

/// 构建器：只暴露常用的可调项，其余走 [`Config`]。
pub struct ClientBuilder {
    config: Config,
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self {
            config: Config::default(),
        }
    }

    /// 完全自定义配置。
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// 单文件分段并发（钳 1..=32）。
    pub fn connections(mut self, connections: usize) -> Self {
        self.config.connections = connections;
        self
    }

    /// 同时执行的任务数（钳 1..=32）。
    pub fn job_concurrency(mut self, concurrency: usize) -> Self {
        self.config.job_concurrency = concurrency;
        self
    }

    pub fn retry_attempts(mut self, attempts: usize) -> Self {
        self.config.retry_attempts = attempts;
        self
    }

    pub fn retry_wait(mut self, wait: Duration) -> Self {
        self.config.retry_wait = wait;
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.config.connect_timeout = timeout;
        self
    }

    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.config.read_timeout = timeout;
        self
    }

    /// ffmpeg 路径（合并 DASH / 校验产物 / 截帧）。
    pub fn ffmpeg(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.ffmpeg = Some(path.into().to_string_lossy().into_owned());
        self
    }

    /// 显式代理；不设置时沿用系统代理。
    pub fn proxy(mut self, proxy: impl Into<String>) -> Self {
        self.config.proxy = Some(proxy.into());
        self
    }

    /// 强制直连（忽略系统代理）。
    pub fn no_proxy(mut self) -> Self {
        self.config.no_proxy = true;
        self
    }

    /// 绑定物理网卡。
    pub fn interface(mut self, interface: impl Into<String>) -> Self {
        self.config.interface = Some(interface.into());
        self
    }

    /// 自定义 DNS（配合 `interface` 绕开隧道 fake-IP）。
    pub fn dns(mut self, resolver: Arc<dyn DnsResolver>) -> Self {
        self.config.dns = Some(resolver);
        self
    }

    /// 全局显式请求头（任务级 `headers` 优先）。
    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.config.headers = headers;
        self
    }

    /// 是否校验产物；关闭后只做大小检查。
    pub fn verify(mut self, verify: bool) -> Self {
        self.config.verify = verify;
        self
    }

    /// 是否计算 MD5。
    pub fn checksum(mut self, checksum: bool) -> Self {
        self.config.checksum = checksum;
        self
    }

    pub fn build(self) -> Result<Client, JobError> {
        Ok(Client {
            engine: Engine::new(self.config)?,
        })
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// SDK 门面。
pub struct Client {
    engine: Engine,
}

impl Client {
    /// 默认配置的客户端。
    pub fn new() -> Result<Self, JobError> {
        Self::builder().build()
    }

    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// 生效配置。
    pub fn config(&self) -> &Config {
        self.engine.config()
    }

    /// 底层执行器（需要更低层能力时直接拿）。
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// 执行一条任务。
    pub async fn run(&self, job: &Job) -> Outcome {
        self.engine.run(job).await
    }

    /// 执行一条任务并回报进度。
    pub async fn run_with_progress(
        &self,
        job: &Job,
        progress: Option<&dyn ProgressSink>,
    ) -> Outcome {
        self.engine.run_with_progress(job, progress).await
    }

    /// 批量执行（并发受 `job_concurrency` 限制），按完成顺序回调。
    pub async fn run_stream(
        &self,
        jobs: &[Job],
        progress: Option<&dyn ProgressSink>,
        on_outcome: &mut dyn FnMut(Outcome),
    ) {
        self.engine.run_stream(jobs, progress, on_outcome).await
    }

    /// 批量执行并收集结果（与输入同序）。
    pub async fn run_all(&self, jobs: &[Job]) -> Vec<Outcome> {
        self.engine.run_all(jobs).await
    }

    /// 纯文件下载（封面、音频等附属文件）。
    pub async fn fetch(
        &self,
        url: &str,
        dest: &std::path::Path,
        headers: &HashMap<String, String>,
        page_url: Option<&str>,
    ) -> Result<crate::transfer::progress::TransferReport, crate::error::DownloadError> {
        self.engine.fetch(url, dest, headers, page_url, None).await
    }
}
