//! 运行配置：一次调用里的全部可调参数。默认值为常用配置，链路表现仍需实测。
//!
//! 不读任何环境变量——环境变量是各项目自己的约定，调用方按需翻译成本结构。
//! 命令行里同名参数一一对应，SDK 与可执行文件行为一致。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::net::DnsResolver;

/// 单文件分段并发默认值（aria2 `--split=8` / `--max-connection-per-server=8` 口径）。
pub const DEFAULT_CONNECTIONS: usize = 8;
/// 分段并发上限（钳 1..=32）。
pub const MAX_CONNECTIONS: usize = 32;
/// 并发下载的任务数默认值。
pub const DEFAULT_JOB_CONCURRENCY: usize = 3;
/// 任务并发上限（钳 1..=32）。
pub const MAX_JOB_CONCURRENCY: usize = 32;
/// 单次尝试总数默认值（含首次）。
pub const DEFAULT_RETRY_ATTEMPTS: usize = 3;
/// 两次重试之间的等待（aria2 `--retry-wait=2` 口径）。
pub const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(2);
/// 连接超时。
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 空闲读超时（网络卡住自己重试；不设整档总超时——大文件合法慢）。
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// 全局连接预算（多任务 × 分段 × HLS 的天花板）。
pub const DEFAULT_GLOBAL_CONNECTIONS: usize = 32;
/// 单 host 连接预算。
pub const DEFAULT_HOST_CONNECTIONS: usize = 16;
/// 产物可信下限：更小视为错误页/空文件。
pub const DEFAULT_MIN_OUTPUT_BYTES: u64 = 16 * 1024;

fn clamp_at_least_one(value: usize, max: usize) -> usize {
    value.clamp(1, max)
}

/// 一次运行的配置。`Default` 就是生产默认值，命令行可按字段覆盖。
#[derive(Clone)]
pub struct Config {
    /// 单文件分段并发连接上限（钳 1..=32）；默认媒体下载会按文件大小与来源下调。
    pub connections: usize,
    /// 同时在跑的任务数（批量下载用，钳 1..=32）。
    pub job_concurrency: usize,
    /// 单次尝试总数（含首次，至少 1）。
    pub retry_attempts: usize,
    /// 两次重试之间的等待。
    pub retry_wait: Duration,
    /// 连接超时。
    pub connect_timeout: Duration,
    /// 空闲读超时。
    pub read_timeout: Duration,
    /// 持续低速保护，默认关闭；有可信备用链接时可按链路能力显式开启。
    pub low_speed_limit: Option<crate::transfer::watchdog::LowSpeedPolicy>,
    /// 全局连接预算（`None` = 32，钳 1..=128）。
    pub global_connections: Option<usize>,
    /// 单 host 连接预算（`None` = 16，钳 1..=64）。
    pub host_connections: Option<usize>,
    /// ffmpeg 可执行文件路径；`None` = 同目录 `resources/ffmpeg[.exe]` → `PATH`。
    pub ffmpeg: Option<String>,
    /// 代理地址（`http://` / `socks5://`）；`None` = 系统代理（环境变量/系统设置）。
    pub proxy: Option<String>,
    /// 显式直连：忽略任何代理配置。
    pub no_proxy: bool,
    /// 绑定物理网卡（多网卡/隧道并存时强制走指定出口）。
    pub interface: Option<String>,
    /// 自定义 DNS（配合 `interface` 绕开隧道 DNS 劫持）。
    pub dns: Option<Arc<dyn DnsResolver>>,
    /// 任务级显式请求头的一部分，优先级低于任务自身 `headers`。
    pub headers: HashMap<String, String>,
    /// 是否用 ffmpeg 校验产物（关闭后只做大小检查）。
    pub verify: bool,
    /// 产物可信下限（字节）。
    pub min_output_bytes: u64,
    /// 是否对产物算 MD5（大文件可关）。
    pub checksum: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            connections: DEFAULT_CONNECTIONS,
            job_concurrency: DEFAULT_JOB_CONCURRENCY,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            retry_wait: DEFAULT_RETRY_WAIT,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
            low_speed_limit: None,
            global_connections: None,
            host_connections: None,
            ffmpeg: None,
            proxy: None,
            no_proxy: false,
            interface: None,
            dns: None,
            headers: HashMap::new(),
            verify: true,
            min_output_bytes: DEFAULT_MIN_OUTPUT_BYTES,
            checksum: true,
        }
    }
}

impl Config {
    /// 规范化为内核可用的取值（钳制越界、去空白），非法组合返回错误。
    pub fn normalized(&self) -> Result<Config, String> {
        let mut config = self.clone();
        config.connections = clamp_at_least_one(self.connections, MAX_CONNECTIONS);
        config.job_concurrency = clamp_at_least_one(self.job_concurrency, MAX_JOB_CONCURRENCY);
        config.retry_attempts = self.retry_attempts.max(1);
        if config
            .low_speed_limit
            .is_some_and(|policy| policy.bytes_per_second == 0 || policy.window.is_zero())
        {
            return Err("慢速保护阈值和采样窗口必须大于零".into());
        }
        config.interface = self
            .interface
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if let Some(interface) = config.interface.as_deref() {
            crate::net::validate_interface(interface)?;
        }
        config.proxy = self
            .proxy
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if config.proxy.is_some() && config.no_proxy {
            return Err("proxy 与 no_proxy 不能同时设置".into());
        }
        config.ffmpeg = self
            .ffmpeg
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        Ok(config)
    }

    /// 单 host / 全局预算的最终取值。
    pub(crate) fn budget_limits(&self) -> (usize, usize) {
        (
            self.global_connections
                .unwrap_or(DEFAULT_GLOBAL_CONNECTIONS)
                .clamp(1, 128),
            self.host_connections
                .unwrap_or(DEFAULT_HOST_CONNECTIONS)
                .clamp(1, 64),
        )
    }
}
