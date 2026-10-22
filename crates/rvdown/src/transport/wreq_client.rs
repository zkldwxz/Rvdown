//! wreq 默认实现：Chrome131 指纹伪装（BoringSSL/JA3），解决抖音 CDN 对非浏览器
//! TLS 栈的 403（aria2/Node fetch 都被拒，wreq 伪装 Chrome 直连）。
//!
//! 这是 `SegmentDownloader` 唯一的默认实现；换 HTTP 库只改本文件，不动 trait 与编排层。
//! 超时口径：connect 15s + read（空闲读）30s——对齐 aria2 `--timeout` 的「网络卡住自己重试」，
//! 不设整档总超时（大文件合法慢）。
//!
//! **手工跟随重定向**（`Policy::none()` + 最多 10 跳）：每跳要按最终目标重算 Referer，
//! 且跨 origin 必须丢掉 `Authorization` / `Cookie`。
//! wreq 默认的自动重定向做不到这两点。
//!
//! 请求头来源分两种模式：
//! - **带上下文**（`with_header_context` / `with_client(…, Some(ctx))`）：调用方传的扁平头
//!   只需要是**与 URL 无关的静态层**（`HeaderSources::static_headers`：显式 + 解析下发）。
//!   每跳都会按该跳目标 URL 重新推断自动头，所以初始 hop 的自动 Referer 绝不可能锁死
//!   最终 hop 的平台规则。
//! - **无上下文**（`new()`，旧扁平调用）：形参里的头被原样当作「调用方已定稿」发送，
//!   本层无法分辨哪一条是自动推断出来的，因此**不能**保证重定向后自动头会重算
//!   （显式 Referer 会按调用方意图保留，跨 origin 仍然丢凭据）。这条路径保持兼容，
//!   需要完整的来源分离时走上下文模式。

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use wreq::header::{HeaderName, HeaderValue};
use wreq::{Client, Method, Response};
use wreq_util::Profile;

use crate::headers::{insert_ci, remove_ci, resolve_request_headers, HeaderSources, HeaderTarget};

use super::identity::{ContentRange, ResourceIdentity};
use super::plan::SegmentRange;
use super::segment::{
    HeadInfo, ProbeOutcome, RangeProbeOutcome, ScopedResponse, SegmentDownloader, SegmentResponse,
    TransportError,
};

/// 指纹档位：Chrome131，与下载 UA 矩阵（request_headers::DEFAULT_DOWNLOAD_USER_AGENT
/// 的 Chrome/131）同一大版本，避免「TLS 指纹是 131、UA 头却是别的版本」这类跨版本破绽。
pub const WREQ_EMULATION: Profile = Profile::Chrome131;

/// 重定向上限（aria2 口径 20 内；10 跳足够应付 CDN 链式跳转）。
pub const MAX_REDIRECTS: usize = 10;

/// 每跳重算自动头所需的上下文。
///
/// 只有 `page_url` 是全局信息：显式/解析下发的头由调用方逐任务经 `headers` 形参传入
/// （它们本来就与任务绑定，不属于全局上下文）。带了这个上下文就表示
/// 「形参里的头是静态层，自动头请你自己按每一跳的目标 URL 推断」。
#[derive(Debug, Clone, Default)]
pub struct HeaderContext {
    /// 原始内容页 URL：平台规则识别不了目标域时用它的 origin 兜底 Referer。
    pub page_url: Option<String>,
}

/// 传输层配置：显式传参，不读任何环境变量。
#[derive(Clone, Default)]
pub struct TransportConfig {
    /// 连接超时（默认 15s）。
    pub connect_timeout: Option<Duration>,
    /// 空闲读超时（默认 30s）。
    pub read_timeout: Option<Duration>,
    /// 显式代理（`http://` / `socks5://`）；`None` = 系统代理。
    pub proxy: Option<String>,
    /// 忽略任何代理配置，强制直连。
    pub no_proxy: bool,
    /// 绑定物理网卡（回环地址不绑定）。
    pub interface: Option<String>,
    /// 自定义 DNS；配合 `interface` 绕开隧道 fake-IP。
    pub dns: Option<std::sync::Arc<dyn crate::net::DnsResolver>>,
}

pub struct WreqSegmentDownloader {
    client: Client,
    header_context: Option<HeaderContext>,
    interface: Option<String>,
}

/// 把 `DnsResolver` 桥接成 wreq 的解析接口。
struct BridgeDns(std::sync::Arc<dyn crate::net::DnsResolver>);
impl wreq::dns::Resolve for BridgeDns {
    fn resolve(&self, name: wreq::dns::Name) -> wreq::dns::Resolving {
        let host = name.as_str().to_string();
        let resolver = self.0.clone();
        Box::pin(async move {
            let addresses = resolver
                .resolve(host)
                .await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(Box::new(addresses.into_iter()) as wreq::dns::Addrs)
        })
    }
}

impl WreqSegmentDownloader {
    /// 每任务只换页面上下文，共享连接池与 TLS 配置，不能串用可变全局 Referer。
    pub fn for_page(&self, page_url: Option<String>) -> Self {
        Self {
            client: self.client.clone(),
            header_context: Some(HeaderContext { page_url }),
            interface: self.interface.clone(),
        }
    }

    /// 默认配置（系统代理 + 默认超时）。
    pub fn new() -> wreq::Result<Self> {
        Self::with_config(&TransportConfig::default(), None)
    }

    /// 按配置构造。带 `HeaderContext` 时形参里的头会被当作静态层，
    /// 每跳按目标 URL 重新推断自动头（Referer）。
    pub fn with_config(
        config: &TransportConfig,
        header_context: Option<HeaderContext>,
    ) -> wreq::Result<Self> {
        Ok(Self {
            client: Self::build_client(config)?,
            header_context,
            interface: config.interface.clone(),
        })
    }

    /// 注入自备 client（测试专用代理、定制超时等）；其余行为与默认实现一致。
    pub fn with_client(client: Client, header_context: Option<HeaderContext>) -> Self {
        Self {
            client,
            header_context,
            interface: None,
        }
    }

    fn build_client(config: &TransportConfig) -> wreq::Result<Client> {
        let mut builder = Client::builder()
            .emulation(WREQ_EMULATION)
            // 手工跟随重定向：每跳要重算 Referer，并在跨 origin 时丢掉凭据。
            .redirect(wreq::redirect::Policy::none())
            .connect_timeout(
                config
                    .connect_timeout
                    .unwrap_or(crate::config::DEFAULT_CONNECT_TIMEOUT),
            )
            .read_timeout(
                config
                    .read_timeout
                    .unwrap_or(crate::config::DEFAULT_READ_TIMEOUT),
            );
        if let Some(proxy) = config.proxy.as_deref() {
            builder = builder.proxy(wreq::Proxy::all(proxy)?);
        } else if config.no_proxy || config.interface.is_some() {
            // 绑定网卡时必然要直连：代理会把流量重新送回隧道。
            builder = builder.no_proxy();
        }
        if let Some(resolver) = config.dns.clone() {
            builder = builder.dns_resolver(BridgeDns(resolver));
        }
        builder.build()
    }

    /// 某一跳要用的请求头。
    ///
    /// 带上下文：`base` 是静态层（显式 + 解析下发，可能含「空值=不发送」标记），
    /// 自动头（尤其 Referer）按**这一跳**的目标 URL 重新推断——绝不用初始 URL 的
    /// 推断结果去覆盖新目标，否则重定向跨平台时会被旧自动 Referer 锁死。
    ///
    /// 无上下文：原样用调用方给的头（兼容旧扁平调用，见模块头限制说明）。
    fn headers_for(
        &self,
        hop_url: &str,
        base: &HashMap<String, String>,
    ) -> HashMap<String, String> {
        match &self.header_context {
            Some(context) => {
                // 把静态层整体当「显式层」再解析：显式与解析下发在设计上同属
                // URL 无关层，优先级都高于自动推断，重建后语义不变（空值仍被压制）。
                let sources = HeaderSources::from_explicit(base.clone());
                resolve_request_headers(
                    &sources,
                    &HeaderTarget {
                        url: hop_url,
                        page_url: context.page_url.as_deref(),
                    },
                )
                .sent
            }
            None => base.clone(),
        }
    }

    /// 手工跟随重定向：逐跳发请求，跨 origin 丢凭据，返回（最终响应, 最终响应地址）。
    ///
    /// 最终地址供资源域绑定：同一个入口 URL 两次重定向到不同目标时，只有它能把
    /// 两个不同资源区分开（RFC 9110 §7.6 有效请求 URI）。
    async fn send_tracked(
        &self,
        method: Method,
        url: &str,
        range: Option<&SegmentRange>,
        base: &HashMap<String, String>,
    ) -> Result<(Response, String), TransportError> {
        let mut current = url.to_string();
        for hop in 0..=MAX_REDIRECTS {
            let mut headers = self.headers_for(&current, base);
            // 跨 origin 重定向绝不泄漏 Authorization / Cookie。
            if hop > 0 && cross_origin(url, &current) {
                remove_ci(&mut headers, "Authorization");
                remove_ci(&mut headers, "Cookie");
            }
            if let Some(range) = range {
                insert_ci(&mut headers, "Range", range.header_value());
            }
            let builder = match method {
                Method::HEAD => self.client.head(&current),
                _ => self.client.get(&current),
            };
            let binding = crate::net::binding_for(
                self.interface.as_deref(),
                &url::Url::parse(&current)
                    .map_err(|_| TransportError::Network("请求地址无效".into()))?,
            )
            .map_err(TransportError::Network)?;
            let builder = if let Some(interface) = binding {
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                {
                    builder.interface(interface)
                }
                #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                {
                    return Err(TransportError::Network("当前系统不支持显式网卡绑定".into()));
                }
            } else {
                builder
            };
            let response = apply_headers(builder, &headers)
                .send()
                .await
                .map_err(map_wreq_error)?;
            let status = response.status().as_u16();
            if !(300..400).contains(&status) || status == 304 {
                return Ok((response, current));
            }
            let Some(location) = response
                .headers()
                .get(wreq::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                return Ok((response, current));
            };
            if hop == MAX_REDIRECTS {
                return Err(TransportError::Network(format!(
                    "重定向次数超过上限 {MAX_REDIRECTS}"
                )));
            }
            current = resolve_location(&current, location);
        }
        Err(TransportError::Network("重定向次数超过上限".into()))
    }
}

impl Default for WreqSegmentDownloader {
    fn default() -> Self {
        // 编译期已知浏览器预设，构建只在 TLS 初始化失败时出错；让调用方显式处理。
        Self::new().expect("wreq client 构建失败（BoringSSL 初始化异常）")
    }
}

/// 两个 URL 是否跨 origin（scheme / host / port 任一不同）。
pub fn cross_origin(from: &str, to: &str) -> bool {
    match (url::Url::parse(from), url::Url::parse(to)) {
        (Ok(a), Ok(b)) => {
            a.scheme() != b.scheme()
                || a.host_str() != b.host_str()
                || a.port_or_known_default() != b.port_or_known_default()
        }
        _ => true,
    }
}

/// 相对 Location 解析到绝对 URL。
fn resolve_location(base: &str, location: &str) -> String {
    match (url::Url::parse(base), location) {
        (Ok(base), location) => base
            .join(location)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| location.to_string()),
        _ => location.to_string(),
    }
}

fn map_wreq_error(error: wreq::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Network(error.to_string())
    }
}

fn apply_headers(
    builder: wreq::RequestBuilder,
    headers: &HashMap<String, String>,
) -> wreq::RequestBuilder {
    let mut builder = builder;
    for (key, value) in headers {
        // 空值 = 明确不发送该头（`HeaderSources` 的压制标记）：绝不能发出去
        // 变成 `Referer: ` 这种空头，那既不符合语义又可能被 CDN 判定异常。
        if value.is_empty() {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(key.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
}

fn header_str<'a>(response: &'a Response, name: &'static str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn content_length_of(response: &Response) -> Option<u64> {
    // 不能用 Response::content_length()：它读的是 body size hint，HEAD 响应恒为 0
    // （wreq 文档明确 HEAD 场景要用 headers 方法）。
    header_str(response, "content-length").and_then(|value| value.trim().parse::<u64>().ok())
}

fn identity_of(response: &Response, length: u64) -> ResourceIdentity {
    ResourceIdentity::new(
        header_str(response, "etag").map(|value| value.to_string()),
        header_str(response, "last-modified").map(|value| value.to_string()),
        length,
    )
}

fn retry_after_of(response: &Response) -> Option<Duration> {
    // 只认 delta-seconds：HTTP-date 形态解析依赖当前时钟，夹不准时宁可退指数退避。
    let raw = header_str(response, "retry-after")?.trim();
    raw.parse::<u64>().ok().map(Duration::from_secs)
}

#[async_trait]
impl SegmentDownloader for WreqSegmentDownloader {
    async fn probe_head(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<HeadInfo, TransportError> {
        let (response, _final_url) = self.send_tracked(Method::HEAD, url, None, headers).await?;
        let status = response.status().as_u16();
        let content_length = content_length_of(&response);
        let accept_ranges = header_str(&response, "accept-ranges")
            .map(|value| value.to_ascii_lowercase().contains("bytes"))
            .unwrap_or(false);
        Ok(HeadInfo {
            status,
            content_length,
            identity: identity_of(&response, content_length.unwrap_or(0)),
            accept_ranges,
        })
    }

    async fn probe(
        &self,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<ProbeOutcome, TransportError> {
        let (response, final_url) = self.send_tracked(Method::HEAD, url, None, headers).await?;
        let status = response.status().as_u16();
        let content_length = content_length_of(&response);
        let accept_ranges = header_str(&response, "accept-ranges")
            .map(|value| value.to_ascii_lowercase().contains("bytes"))
            .unwrap_or(false);
        Ok(ProbeOutcome {
            head: HeadInfo {
                status,
                content_length,
                identity: identity_of(&response, content_length.unwrap_or(0)),
                accept_ranges,
            },
            effective_url: Some(final_url),
        })
    }

    /// HEAD 被拒/无长度时的 0-0 范围探测：GET 也走同一条重定向链，
    /// 最终地址与 HEAD 口径一致（否则两条探测路径会把资源域绑到不同的 URL）。
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
        let (response, final_url) = self
            .send_tracked(Method::GET, url, Some(&probe), headers)
            .await
            .ok()?;
        if response.status().as_u16() != 206 {
            return None;
        }
        let content_range = header_str(&response, "content-range").and_then(ContentRange::parse);
        let total = content_range.and_then(|range| range.total())?;
        if total == 0 {
            return None;
        }
        let mut identity = identity_of(&response, total);
        if identity.length == 0 {
            identity.length = total;
        }
        Some(RangeProbeOutcome {
            total,
            identity,
            effective_url: Some(final_url),
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

    /// 每条 Range GET 都带回本次请求的最终响应地址：探测落到 A、这条请求却跳到 B 时，
    /// 编排层要靠它发现「换了资源」，而不是把 B 的字节拼进 A 的产物。
    async fn get_scoped(
        &self,
        url: &str,
        range: Option<&SegmentRange>,
        headers: &HashMap<String, String>,
    ) -> Result<ScopedResponse, TransportError> {
        let (response, final_url) = self.send_tracked(Method::GET, url, range, headers).await?;
        let status = response.status().as_u16();
        let content_length = content_length_of(&response);
        let content_range = header_str(&response, "content-range").and_then(ContentRange::parse);
        let total = content_range
            .and_then(|range| range.total())
            .or(content_length);
        let identity = identity_of(&response, total.unwrap_or(0));
        let retry_after = retry_after_of(&response);
        let body = response
            .bytes_stream()
            .map(|item| item.map_err(map_wreq_error).map(|bytes| bytes.to_vec()));
        Ok(ScopedResponse {
            response: SegmentResponse {
                status,
                content_length,
                body: Box::pin(body),
                content_range,
                identity,
                retry_after,
            },
            effective_url: Some(final_url),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::FileDownloader;
    use crate::transport::segment::{RangeDownloader, RangeDownloaderConfig};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // 回环假服务器（127.0.0.1，进程内线程）：HTTP/1.1 每连接一请求，
    // 支持 HEAD / Range GET / 200-回退 / 403，记录请求头供断言。禁外网。

    #[derive(Default, Clone)]
    struct ServerScript {
        body: Vec<u8>,
        ignore_range: bool,
        forbid: bool,
        head_status: u16,
    }

    #[derive(Default)]
    struct ServerLog {
        ranges: Mutex<Vec<String>>,
        request_headers: Mutex<Vec<Vec<(String, String)>>>,
        head_calls: Mutex<u32>,
    }

    fn spawn_server(script: ServerScript) -> (String, Arc<ServerLog>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(ServerLog::default());
        let log_for_thread = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let script = script.clone();
                let log = Arc::clone(&log_for_thread);
                std::thread::spawn(move || handle_connection(stream, script, log));
            }
        });
        (format!("http://{addr}/video.mp4"), log)
    }

    fn handle_connection(mut stream: TcpStream, script: ServerScript, log: Arc<ServerLog>) {
        let mut buffer = [0u8; 8192];
        let mut raw = Vec::new();
        // 读到请求头结束（GET/HEAD 无请求体）。
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buffer[..n]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => return,
            }
        }
        let text = String::from_utf8_lossy(&raw).to_string();
        let mut lines = text.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        log.request_headers.lock().unwrap().push(headers.clone());
        let method = request_line
            .split(' ')
            .next()
            .unwrap_or_default()
            .to_string();
        let range = headers
            .iter()
            .find(|(name, _)| name == "range")
            .map(|(_, value)| value.clone());

        let mut response = Vec::new();
        let total = script.body.len();
        match method.as_str() {
            "HEAD" => {
                *log.head_calls.lock().unwrap() += 1;
                let status = if script.head_status != 0 {
                    script.head_status
                } else {
                    200
                };
                response.extend_from_slice(
                    format!(
                        "HTTP/1.1 {status} OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                );
            }
            "GET" => {
                log.ranges
                    .lock()
                    .unwrap()
                    .push(range.clone().unwrap_or_else(|| "none".into()));
                if script.forbid {
                    response.extend_from_slice(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                } else if let Some(range) = range.filter(|_| !script.ignore_range) {
                    let (start, end) = parse_range(&range, total);
                    let length = end - start + 1;
                    response.extend_from_slice(
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{total}\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                    response.extend_from_slice(&script.body[start..=end]);
                } else {
                    response.extend_from_slice(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                    response.extend_from_slice(&script.body);
                }
            }
            _ => {
                response.extend_from_slice(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        }
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    }

    fn parse_range(header: &str, total: usize) -> (usize, usize) {
        let spec = header.trim_start_matches("bytes=");
        let (start, end) = spec.split_once('-').unwrap_or((spec, ""));
        let start: usize = start.parse().unwrap_or(0);
        let end: usize = end
            .parse::<usize>()
            .map_or(total.saturating_sub(1), |value: usize| value.min(total - 1));
        (start.min(total - 1), end.max(start))
    }

    fn config(attempts: usize) -> RangeDownloaderConfig {
        RangeDownloaderConfig {
            connections: 8,
            retry_attempts: attempts,
            retry_wait: Duration::from_millis(10),
            sleep: Arc::new(|_| Box::pin(futures::future::ready(()))),
        }
    }

    fn headers() -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert(
            "User-Agent".to_string(),
            crate::headers::DEFAULT_DOWNLOAD_USER_AGENT.to_string(),
        );
        headers.insert("Referer".to_string(), "https://www.douyin.com/".to_string());
        headers
    }

    // ---- 测试专用 HTTP 代理：把虚拟平台域名映射到回环（不改系统 hosts） ----

    #[derive(Clone)]
    struct ProxyScript {
        /// 命中 `(host, path)` 的请求回 302 到 `redirect_to`。
        redirect_from: (String, String),
        redirect_to: String,
        redirect_status: u16,
        /// 其余请求直接回这份内容（HEAD/GET/Range 都支持）。
        body: Vec<u8>,
        etag: String,
    }

    impl Default for ProxyScript {
        fn default() -> Self {
            Self {
                redirect_from: ("v26-web.douyinvod.com".into(), "/a.mp4".into()),
                redirect_to: "http://upos-sz-mirror.bilivideo.com/b.mp4".into(),
                redirect_status: 302,
                body: Vec::new(),
                etag: r#""v1""#.into(),
            }
        }
    }

    /// 代理日志的一条：方法 + 完整目标 URL + 请求头。
    type ProxyRequest = (String, String, Vec<(String, String)>);

    #[derive(Default)]
    struct ProxyLog {
        requests: Mutex<Vec<ProxyRequest>>,
    }

    impl ProxyLog {
        fn find(&self, url_suffix: &str) -> Option<(String, Vec<(String, String)>)> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .find(|(_, url, _)| url.ends_with(url_suffix))
                .map(|(method, _, headers)| (method.clone(), headers.clone()))
        }

        fn seen_urls(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(_, url, _)| url.clone())
                .collect()
        }
    }

    fn header_of(headers: &[(String, String)], name: &str) -> Option<String> {
        headers
            .iter()
            .find(|(key, _)| key == &name.to_ascii_lowercase())
            .map(|(_, value)| value.clone())
    }

    /// 起一个测试专用 HTTP 代理：客户端用 `Proxy::all` 指过来，
    /// 请求行是绝对 URI（`GET http://虚拟域名/path HTTP/1.1`），代理直接应答。
    fn spawn_proxy(script: ProxyScript) -> (String, Arc<ProxyLog>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(ProxyLog::default());
        let log_for_thread = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let script = script.clone();
                let log = Arc::clone(&log_for_thread);
                std::thread::spawn(move || proxy_connection(stream, script, log));
            }
        });
        (format!("http://{addr}"), log)
    }

    fn proxy_connection(mut stream: TcpStream, script: ProxyScript, log: Arc<ProxyLog>) {
        let mut buffer = [0u8; 8192];
        let mut raw = Vec::new();
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buffer[..n]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => return,
            }
        }
        let text = String::from_utf8_lossy(&raw).to_string();
        let mut lines = text.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut parts = request_line.split(' ');
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
            }
        }
        log.requests
            .lock()
            .unwrap()
            .push((method.clone(), target.clone(), headers.clone()));

        let parsed = url::Url::parse(&target).ok();
        let host = parsed
            .as_ref()
            .and_then(|url| url.host_str())
            .unwrap_or_default()
            .to_string();
        let path = parsed
            .as_ref()
            .map(|url| url.path().to_string())
            .unwrap_or_default();

        let mut response = Vec::new();
        if host == script.redirect_from.0 && path == script.redirect_from.1 {
            response.extend_from_slice(
                format!(
                    "HTTP/1.1 {} Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    script.redirect_status, script.redirect_to
                )
                .as_bytes(),
            );
        } else {
            let total = script.body.len();
            let range = header_of(&headers, "range");
            match method.as_str() {
                "HEAD" => response.extend_from_slice(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: {}\r\nConnection: close\r\n\r\n",
                        script.etag
                    )
                    .as_bytes(),
                ),
                _ => match range {
                    Some(range) => {
                        let (start, end) = parse_range(&range, total);
                        let length = end - start + 1;
                        response.extend_from_slice(
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{total}\r\nETag: {}\r\nConnection: close\r\n\r\n",
                                script.etag
                            )
                            .as_bytes(),
                        );
                        response.extend_from_slice(&script.body[start..=end]);
                    }
                    None => {
                        response.extend_from_slice(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nETag: {}\r\nConnection: close\r\n\r\n",
                                script.etag
                            )
                            .as_bytes(),
                        );
                        response.extend_from_slice(&script.body);
                    }
                },
            }
        }
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    }

    fn proxied_client(proxy: &str) -> Client {
        Client::builder()
            .emulation(WREQ_EMULATION)
            .redirect(wreq::redirect::Policy::none())
            .proxy(wreq::Proxy::all(proxy).expect("代理 URI 合法"))
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(5))
            .build()
            .expect("client")
    }

    fn static_headers() -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert(
            "User-Agent".to_string(),
            crate::headers::DEFAULT_DOWNLOAD_USER_AGENT.to_string(),
        );
        headers
    }

    // 显式空值 = 明确不发送该头：拿到了上下文也不能把它当普通值发出去。
    #[test]
    fn 显式空referer不进入发送头表() {
        let downloader = WreqSegmentDownloader::with_config(
            &TransportConfig::default(),
            Some(HeaderContext { page_url: None }),
        )
        .unwrap();
        let mut base = static_headers();
        base.insert("Referer".to_string(), String::new());

        let resolved = downloader.headers_for("https://upos-sz-mirror.bilivideo.com/b.mp4", &base);
        assert!(
            !resolved.contains_key("Referer"),
            "显式空值表示不发 Referer，不能被自动推断顶回来: {resolved:?}"
        );
    }

    #[test]
    fn 上下文路径每跳按目标url重算referer且显式优先() {
        let downloader = WreqSegmentDownloader::with_config(
            &TransportConfig::default(),
            Some(HeaderContext { page_url: None }),
        )
        .unwrap();

        let first =
            downloader.headers_for("https://v26-web.douyinvod.com/a.mp4", &static_headers());
        assert_eq!(
            first.get("Referer").map(String::as_str),
            Some("https://www.douyin.com/"),
            "首跳按抖音目标推断"
        );

        let final_hop = downloader.headers_for(
            "https://upos-sz-mirror.bilivideo.com/b.mp4",
            &static_headers(),
        );
        assert_eq!(
            final_hop.get("Referer").map(String::as_str),
            Some("https://www.bilibili.com/"),
            "最终 hop 必须按新目标重算，绝不被初始 hop 的自动 Referer 锁死"
        );

        let mut explicit = static_headers();
        explicit.insert("Referer".to_string(), "https://user.example/".to_string());
        let hop = downloader.headers_for("https://upos-sz-mirror.bilivideo.com/b.mp4", &explicit);
        assert_eq!(
            hop.get("Referer").map(String::as_str),
            Some("https://user.example/"),
            "用户显式 Referer 跨跳保留，不被平台规则改写"
        );
    }

    #[tokio::test]
    async fn 跨平台重定向自动referer按最终目标重算且跨域凭据不发() {
        // 目标域名在平台规则表内（抖音 → B 站），用测试专用 HTTP 代理把虚拟域名
        // 映射到回环：不改系统 hosts，也不碰外网。
        let body: Vec<u8> = (0..(64 * 1024) as u32).map(|i| (i % 241) as u8).collect();
        let (proxy, log) = spawn_proxy(ProxyScript {
            body: body.clone(),
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");

        let client = proxied_client(&proxy);
        let downloader = Arc::new(WreqSegmentDownloader::with_client(
            client,
            Some(HeaderContext { page_url: None }),
        ));
        let mut headers = static_headers();
        headers.insert("Cookie".to_string(), "sid=1".to_string());
        headers.insert("Authorization".to_string(), "Bearer token".to_string());
        RangeDownloader::new(downloader, config(2))
            .download("http://v26-web.douyinvod.com/a.mp4", &dest, &headers)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body, "经重定向拿到成片");

        let (_, first) = log.find("/a.mp4").expect("首跳请求");
        assert_eq!(
            header_of(&first, "referer").as_deref(),
            Some("https://www.douyin.com/"),
            "首跳按抖音目标带 Referer"
        );
        assert_eq!(
            header_of(&first, "cookie").as_deref(),
            Some("sid=1"),
            "同 origin 保留 Cookie"
        );

        let (_, final_hop) = log.find("/b.mp4").expect("最终跳请求");
        assert_eq!(
            header_of(&final_hop, "referer").as_deref(),
            Some("https://www.bilibili.com/"),
            "最终 hop 的 Referer 按 B 站目标重算，绝不被抖音自动头锁死"
        );
        assert_eq!(
            header_of(&final_hop, "cookie"),
            None,
            "跨 origin 绝不发 Cookie"
        );
        assert_eq!(
            header_of(&final_hop, "authorization"),
            None,
            "跨 origin 绝不发 Authorization"
        );
        let urls = log.seen_urls();
        assert!(
            urls.iter().any(|url| url.contains("v26-web.douyinvod.com")),
            "确实走了重定向: {urls:?}"
        );
    }

    #[tokio::test]
    async fn 显式referer跨重定向保留() {
        let body = vec![3u8; 32 * 1024];
        let (proxy, log) = spawn_proxy(ProxyScript {
            body: body.clone(),
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");

        let client = proxied_client(&proxy);
        let downloader = Arc::new(WreqSegmentDownloader::with_client(
            client,
            Some(HeaderContext { page_url: None }),
        ));
        let mut headers = static_headers();
        headers.insert("Referer".to_string(), "https://user.example/".to_string());
        RangeDownloader::new(downloader, config(2))
            .download("http://v26-web.douyinvod.com/a.mp4", &dest, &headers)
            .await
            .unwrap();

        let (_, final_hop) = log.find("/b.mp4").expect("最终跳请求");
        assert_eq!(
            header_of(&final_hop, "referer").as_deref(),
            Some("https://user.example/"),
            "用户显式 Referer 最高优先，重定向后也不被平台规则改写"
        );
    }

    #[tokio::test]
    async fn 显式空referer绝不在线上发空值头() {
        let body = vec![5u8; 32 * 1024];
        let (proxy, log) = spawn_proxy(ProxyScript {
            body,
            redirect_from: ("v26-web.douyinvod.com".into(), "/a.mp4".into()),
            redirect_to: "http://upos-sz-mirror.bilivideo.com/b.mp4".into(),
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");

        let client = proxied_client(&proxy);
        let downloader = Arc::new(WreqSegmentDownloader::with_client(
            client,
            Some(HeaderContext { page_url: None }),
        ));
        let mut headers = static_headers();
        headers.insert("Referer".to_string(), String::new());
        RangeDownloader::new(downloader, config(2))
            .download("http://v26-web.douyinvod.com/a.mp4", &dest, &headers)
            .await
            .unwrap();

        for (method, url) in [("HEAD", "/b.mp4"), ("GET", "/b.mp4")] {
            if let Some((got_method, hop)) = log.find(url).filter(|(got, _)| got == method) {
                let _ = got_method;
                assert_eq!(
                    header_of(&hop, "referer"),
                    None,
                    "显式空值 = 不发 Referer，{method} 也不能出现空值头"
                );
            }
        }
    }

    #[tokio::test]
    async fn wreq_downloads_segments_over_loopback_with_custom_headers() {
        let body: Vec<u8> = (0..(2 * 1024 * 1024 + 512 * 1024) as u32)
            .map(|i| (i % 251) as u8)
            .collect();
        let (url, log) = spawn_server(ServerScript {
            body: body.clone(),
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");

        let downloader =
            RangeDownloader::new(Arc::new(WreqSegmentDownloader::new().unwrap()), config(2));
        let transferred = downloader.download(&url, &dest, &headers()).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body, "分段拼装结果与源一致");
        assert_eq!(transferred, body.len() as u64);
        assert_eq!(*log.head_calls.lock().unwrap(), 1, "先 HEAD 预检");
        let mut ranges = log.ranges.lock().unwrap().clone();
        ranges.sort();
        assert_eq!(
            ranges,
            vec![
                "bytes=0-1310719".to_string(),
                "bytes=1310720-2621439".to_string()
            ],
            "2.5M 切 2 段"
        );
        // 显式头必须盖过 emulation 默认：服务器看到我们的 UA 与 Referer。
        let first_get = &log.request_headers.lock().unwrap()[1];
        let ua = first_get
            .iter()
            .find(|(name, _)| name == "user-agent")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        assert_eq!(ua, crate::headers::DEFAULT_DOWNLOAD_USER_AGENT);
        assert!(first_get
            .iter()
            .any(|(name, value)| name == "referer" && value == "https://www.douyin.com/"));
    }

    #[tokio::test]
    async fn wreq_small_file_streams_without_range() {
        let body = vec![42u8; 100 * 1024];
        let (url, log) = spawn_server(ServerScript {
            body: body.clone(),
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("small.mp4");

        let downloader =
            RangeDownloader::new(Arc::new(WreqSegmentDownloader::new().unwrap()), config(2));
        downloader.download(&url, &dest, &headers()).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(*log.ranges.lock().unwrap(), vec!["none".to_string()]);
    }

    #[tokio::test]
    async fn wreq_falls_back_to_stream_when_server_ignores_range() {
        let body: Vec<u8> = (0..(2 * 1024 * 1024 + 512 * 1024) as u32)
            .map(|i| (i % 247) as u8)
            .collect();
        let (url, log) = spawn_server(ServerScript {
            body: body.clone(),
            ignore_range: true,
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");

        let downloader =
            RangeDownloader::new(Arc::new(WreqSegmentDownloader::new().unwrap()), config(1));
        downloader.download(&url, &dest, &headers()).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(
            log.ranges.lock().unwrap().contains(&"none".to_string()),
            "最终必须回退单流"
        );
    }

    #[tokio::test]
    async fn wreq_forbidden_fails_with_status_after_retries() {
        let (url, log) = spawn_server(ServerScript {
            forbid: true,
            head_status: 403,
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");

        let downloader =
            RangeDownloader::new(Arc::new(WreqSegmentDownloader::new().unwrap()), config(2));
        let error = downloader
            .download(&url, &dest, &headers())
            .await
            .unwrap_err();

        assert!(
            matches!(error, crate::error::DownloadError::Http { status: 403, .. }),
            "{error:?}"
        );
        assert_eq!(log.ranges.lock().unwrap().len(), 2, "重试耗尽");
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn wreq_head_rejected_falls_back_to_get() {
        let body = vec![1u8; 64 * 1024];
        let (url, log) = spawn_server(ServerScript {
            body: body.clone(),
            head_status: 405,
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");

        let downloader =
            RangeDownloader::new(Arc::new(WreqSegmentDownloader::new().unwrap()), config(2));
        downloader.download(&url, &dest, &headers()).await.unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), body, "HEAD 被拒不挡下载");
        // 有界回退：先范围探测取长度，再单流。
        assert_eq!(
            *log.ranges.lock().unwrap(),
            vec!["bytes=0-0".to_string(), "none".to_string()]
        );
    }
}
