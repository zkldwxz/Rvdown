//! 出口网络控制：显式网卡绑定与可插拔 DNS。
//!
//! 用途：机器同时挂着 VPN/隧道（TUN 模式常见会把 DNS 劫持成 fake-IP）时，下载仍要
//! 走真实出口。这里的实现**只影响本进程的请求**，不改操作系统路由或代理设置。
//!
//! - `interface`：把连接绑定到指定物理网卡。回环地址（本地服务）不绑定，避免把
//!   测试与本地接口的流量带走。
//! - `dns`：自定义解析实现。隧道 fake-IP 场景可注入 `DohJsonResolver`，按网卡直连
//!   DNS-over-HTTPS 拿真实地址；解析结果 5 分钟内复用。

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use url::Url;

/// 自定义 DNS 解析：返回该域名的全部可用地址。
pub trait DnsResolver: Send + Sync {
    fn resolve(
        &self,
        host: String,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>>;
}

/// 网卡名合法性：只允许字母数字与 `_-`，且拒绝隧道/回环设备。
///
/// 隧道设备（`utun*` / `tun*` / `tap*`）本身就是被绕开的对象；回环设备没有意义。
pub fn validate_interface(interface: &str) -> Result<(), String> {
    if interface.is_empty()
        || !interface
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        || ["utun", "tun", "tap", "lo"]
            .iter()
            .any(|prefix| interface.starts_with(prefix))
    {
        return Err("必须绑定有效物理网卡，禁止隧道或回环网卡".into());
    }
    Ok(())
}

/// 本次请求是否要绑定网卡：配置了网卡、且目标不是本机回环地址时返回网卡名。
///
/// 返回 `Err` 表示网卡名非法——**绝不静默退回代理路由**，否则真实回归会在错误的
/// 出口上「成功」，掩盖网络问题。
pub fn binding_for(interface: Option<&str>, url: &Url) -> Result<Option<String>, String> {
    let Some(interface) = interface else {
        return Ok(None);
    };
    validate_interface(interface)?;
    let local = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    Ok((!local).then(|| interface.to_string()))
}

/// DNS-over-HTTPS（JSON 形态）解析器：请求本身也绑定到同一块网卡，
/// 并用固定 IP 直连 DNS 服务，避免「解析 DNS 又依赖被污染的系统 DNS」。
#[derive(Debug, Clone)]
pub struct DohJsonResolver {
    /// DoH 端点（如 `https://dns.example.com/resolve`）。
    pub endpoint: String,
    /// 端点所在主机名（用于 `--resolve` 固定 IP）。
    pub endpoint_host: String,
    /// 端点的固定 IP（跳过系统解析）。
    pub endpoint_ip: std::net::Ipv4Addr,
    /// 绑定网卡；`None` = 不绑定。
    pub interface: Option<String>,
}

impl DohJsonResolver {
    pub fn new(
        endpoint: impl Into<String>,
        endpoint_host: impl Into<String>,
        endpoint_ip: std::net::Ipv4Addr,
        interface: Option<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            endpoint_host: endpoint_host.into(),
            endpoint_ip,
            interface,
        }
    }

    fn query_blocking(&self, host: String) -> std::io::Result<Vec<SocketAddr>> {
        type Cache = HashMap<(String, String, String), (Instant, Vec<SocketAddr>)>;
        const CACHE_TTL: Duration = Duration::from_secs(300);
        static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

        let interface_key = self.interface.clone().unwrap_or_default();
        let key = (self.endpoint.clone(), interface_key.clone(), host.clone());
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some((_, addresses)) = cache
            .lock()
            .map_err(|_| std::io::Error::other("DNS 缓存不可用"))?
            .get(&key)
            .filter(|(time, _)| time.elapsed() < CACHE_TTL)
        {
            return Ok(addresses.clone());
        }

        let mut endpoint =
            Url::parse(&self.endpoint).map_err(|_| std::io::Error::other("DNS 端点无效"))?;
        endpoint
            .query_pairs_mut()
            .append_pair("name", &host)
            .append_pair("type", "A");

        // 用 curl 直连固定 IP 并绑定网卡：不经过系统 fake-IP 解析，也不给失败留系统回退。
        let mut command = std::process::Command::new("curl");
        if let Some(interface) = self.interface.as_deref() {
            command.args(["--interface", interface]);
        }
        let resolve = format!("{}:443:{}", self.endpoint_host, self.endpoint_ip);
        let output = command
            .args([
                "--noproxy",
                "*",
                "--resolve",
                &resolve,
                "--silent",
                "--show-error",
                "--connect-timeout",
                "5",
                "--max-time",
                "10",
                endpoint.as_str(),
            ])
            .output()
            .map_err(|_| std::io::Error::other("无法启动 DNS 查询"))?;
        if !output.status.success() {
            return Err(std::io::Error::other("DNS 查询失败"));
        }
        let addresses = dns_answers(&String::from_utf8_lossy(&output.stdout))?;
        if let Ok(mut cache) = cache.lock() {
            cache.insert(key, (Instant::now(), addresses.clone()));
        }
        Ok(addresses)
    }
}

impl DnsResolver for DohJsonResolver {
    fn resolve(
        &self,
        host: String,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<SocketAddr>>> + Send>> {
        let this = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || this.query_blocking(host))
                .await
                .map_err(|_| std::io::Error::other("DNS 解析任务失败"))?
        })
    }
}

/// 解析 DoH JSON 响应，只接受真实 IPv4 地址。
///
/// 丢掉保留网段里的 fake-IP（198.18.0.0/15，常见隧道实现的返回），宁可报错也不拿它当真。
pub fn dns_answers(body: &str) -> std::io::Result<Vec<SocketAddr>> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| std::io::Error::other("DNS 响应无效"))?;
    if value["Status"].as_u64() != Some(0) {
        return Err(std::io::Error::other("DNS 查询失败"));
    }
    let addresses: Vec<_> = value["Answer"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["type"].as_u64() == Some(1))
        .filter_map(|row| row["data"].as_str()?.parse::<std::net::Ipv4Addr>().ok())
        .filter(|ip| !(ip.octets()[0] == 198 && [18, 19].contains(&ip.octets()[1])))
        .map(|ip| SocketAddr::new(ip.into(), 0))
        .collect();
    if addresses.is_empty() {
        return Err(std::io::Error::other("DNS 未返回真实 IPv4 地址"));
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 外网绑定物理网卡而回环地址保持本地() {
        assert_eq!(
            binding_for(
                Some("en0"),
                &Url::parse("https://cdn.example.com/video.mp4").unwrap()
            )
            .unwrap(),
            Some("en0".into())
        );
        for address in [
            "http://127.0.0.1:8020",
            "http://localhost:8020",
            "http://[::1]:8020",
        ] {
            assert_eq!(
                binding_for(Some("en0"), &Url::parse(address).unwrap()).unwrap(),
                None
            );
        }
    }

    #[test]
    fn 指定隧道或非法网卡必须拒绝不能静默回到代理路由() {
        for interface in ["utun4", "tun0", "tap1", "en0\n", "", "lo0"] {
            assert!(binding_for(
                Some(interface),
                &Url::parse("https://example.test").unwrap()
            )
            .is_err());
        }
    }

    #[test]
    fn 直连解析只接受真实地址拒绝隧道虚拟地址和空结果() {
        let body = r#"{"Status":0,"Answer":[{"type":5,"data":"alias.example"},{"type":1,"data":"192.0.2.1"}]}"#;
        assert_eq!(dns_answers(body).unwrap()[0].ip().to_string(), "192.0.2.1");
        for body in [
            r#"{"Status":0,"Answer":[{"type":1,"data":"198.18.0.2"}]}"#,
            r#"{"Status":3}"#,
            "bad",
        ] {
            assert!(dns_answers(body).is_err());
        }
    }
}
