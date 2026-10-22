//! 下载请求头：多数平台 CDN 会对缺少 Referer/User-Agent 的直链返回 403
//! （实测 bilibili 的 bilivideo 直链无 Referer 时 403，补上后 206 正常）。

use std::collections::HashMap;

/// 下载 UA 必须保持较新的 Chrome 版本：实测抖音 play 接口对旧版 UA（如 Chrome/126）
/// 直接 403，Chrome/131 起 302 到 CDN 正常。升级 UA 前先验证抖音/头条链路。
pub const DEFAULT_DOWNLOAD_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 平台域名 → Referer 映射（按 hostname 后缀锚定）。
const REFERER_RULES: &[(&[&str], &str)] = &[
    (
        &[
            "bilibili.com",
            "bilivideo.com",
            "bilivideo.cn",
            "hdslb.com",
            "biliapi.net",
        ],
        "https://www.bilibili.com/",
    ),
    (
        &[
            "kuaishou.com",
            "kwaicdn.com",
            "kwimgs.com",
            "chenzhongtech.com",
        ],
        "https://www.kuaishou.com/",
    ),
    (
        &[
            "toutiao.com",
            "toutiaovod.com",
            "toutiaoimg.com",
            "ixiguavideo.com",
            "ixigua.com",
        ],
        "https://www.toutiao.com/",
    ),
    (
        &[
            "douyin.com",
            "douyinpic.com",
            "douyinvod.com",
            "bytedance.com",
        ],
        "https://www.douyin.com/",
    ),
    (
        &["xiaohongshu.com", "xhscdn.com"],
        "https://www.xiaohongshu.com/",
    ),
];

fn host_matches(host: &str, domain: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{}", domain))
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .map(|u| u.host_str().unwrap_or("").to_string())
}

/// 按候选 URL 的域名推断 Referer；找不到返回 None。
pub fn referer_for_urls(urls: &[Option<&str>]) -> Option<String> {
    for url in urls.iter().flatten() {
        let Some(host) = host_of(url) else { continue };
        for (domains, referer) in REFERER_RULES {
            if domains.iter().any(|domain| host_matches(&host, domain)) {
                return Some((*referer).to_string());
            }
        }
    }
    None
}

fn origin_of(page_url: Option<&str>) -> Option<String> {
    let page_url = page_url?;
    let url = url::Url::parse(page_url).ok()?;
    let origin = url.origin().ascii_serialization();
    if origin == "null" {
        None
    } else {
        Some(origin)
    }
}

/// 默认下载头：始终带浏览器 User-Agent；能识别平台时补平台 Referer，
/// 识别不了时退而用原始内容页的 origin 当 Referer，实在没有就只带 UA。
pub fn default_download_headers(
    url_hints: &[Option<&str>],
    page_url: Option<&str>,
) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert(
        "User-Agent".to_string(),
        DEFAULT_DOWNLOAD_USER_AGENT.to_string(),
    );
    if let Some(referer) = referer_for_urls(url_hints).or_else(|| origin_of(page_url)) {
        headers.insert("Referer".to_string(), referer);
    }
    headers
}

/// 合并下载头：默认头打底，解析服务下发的头优先。视频与封面要分别调用。
///
/// 头名大小写无关合并（`REFERER` 与 `Referer` 不会同时发两次）。
pub fn merge_download_headers(
    provided: Option<&HashMap<String, String>>,
    url_hints: &[Option<&str>],
    page_url: Option<&str>,
) -> HashMap<String, String> {
    let sources = HeaderSources {
        explicit: HashMap::new(),
        resolver: provided.cloned().unwrap_or_default(),
    };
    // url_hints 是「候选直链 → 原始内容页」的顺序提示，
    // 这里把第一个可解析的候选当作目标 URL 来推断平台规则。
    let first = url_hints.iter().flatten().next().copied().unwrap_or("");
    resolve_request_headers(
        &sources,
        &HeaderTarget {
            url: first,
            page_url,
        },
    )
    .sent
}

/// 头的来源分层：**自动推断的头绝不冒充用户头**（否则候选回退时会把第一候选的
/// 自动 Referer 锁死给所有备用候选）。
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderSource {
    /// 用户/任务显式传入（含解析服务以外的来源）；合法空值表示「不发送该头」。
    Explicit,
    /// 调用方明确下发的原始头表。
    Resolver,
    /// 按目标 URL / 页面 origin 自动推断。
    Auto,
}

/// 请求头来源分槽：显式与自动分开存，合并时按来源优先级逐层回退。
#[derive(Debug, Clone, Default)]
pub struct HeaderSources {
    /// 用户/任务显式头；值为空串表示**明确不发送**该头。
    pub explicit: HashMap<String, String>,
    /// 解析服务明确下发的原始头（未叠加任何自动推断）。
    pub resolver: HashMap<String, String>,
}

impl HeaderSources {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只有用户显式头。
    pub fn from_explicit(explicit: HashMap<String, String>) -> Self {
        Self {
            explicit,
            resolver: HashMap::new(),
        }
    }

    /// 头名大小写无关地取显式值（`referer` / `REFERER` / `Referer` 等价）。
    pub fn explicit_get(&self, name: &str) -> Option<&str> {
        get_ci(&self.explicit, name)
    }

    /// 头名大小写无关地取解析下发值。
    pub fn resolver_get(&self, name: &str) -> Option<&str> {
        get_ci(&self.resolver, name)
    }

    /// **与目标 URL 无关**的头层（显式 > 解析下发，外加默认 UA）。
    ///
    /// 用途：需要按每一跳/每个候选重算自动头（尤其 Referer）时当基座。
    /// 静态层绝不含按某个初始 URL 推断出来的 Referer——否则重定向到别的平台时
    /// 旧自动 Referer 会锁死新目标的平台规则。
    ///
    /// 显式空值原样保留为空串：它是「明确不发送该头」的压制标记，由
    /// `resolve_request_headers` 消费；直接发请求时 `apply_headers` 会跳过空值。
    pub fn static_headers(&self) -> HashMap<String, String> {
        let mut merged = self.resolver.clone();
        for (name, value) in &self.explicit {
            insert_ci(&mut merged, name, value.clone());
        }
        if get_ci(&merged, "User-Agent").is_none() {
            insert_ci(
                &mut merged,
                "User-Agent",
                DEFAULT_DOWNLOAD_USER_AGENT.to_string(),
            );
        }
        merged
    }
}

/// 一次请求的目标：当前目标 URL 与原始内容页 URL。
#[derive(Debug, Clone, Copy)]
pub struct HeaderTarget<'a> {
    /// 当前请求目标 URL（每个候选 / 音频 / 封面 / HLS 分片各自一次）。
    pub url: &'a str,
    /// 原始内容页 URL（平台规则与 origin 的兜底来源）。
    pub page_url: Option<&'a str>,
}

/// 计算结果：真正要发的头 + 被显式空值压制（不发送）的头名。
#[derive(Debug, Clone, Default)]
pub struct ResolvedHeaders {
    pub sent: HashMap<String, String>,
    pub suppressed: Vec<String>,
}

/// 头名合法性：必须是合法 token（无空白/控制字符），大小写归一。
///
/// 头值不得含 `\r` / `\n` / NUL（防头注入）。非法头**丢弃并记录**，绝不发出去。
fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn valid_header_value(value: &str) -> bool {
    !value.bytes().any(|byte| matches!(byte, b'\r' | b'\n' | 0))
}

fn canonical_name(name: &str) -> String {
    // 已知头用标准大小写，其余保留原样（HTTP 头名大小写无关）。
    match name.to_ascii_lowercase().as_str() {
        "referer" => "Referer".to_string(),
        "user-agent" => "User-Agent".to_string(),
        "cookie" => "Cookie".to_string(),
        "authorization" => "Authorization".to_string(),
        "if-range" => "If-Range".to_string(),
        "range" => "Range".to_string(),
        _ => name.to_string(),
    }
}

/// 头名大小写无关取值。
pub fn get_ci<'a>(map: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    let lower = name.to_ascii_lowercase();
    map.iter()
        .find(|(key, _)| key.to_ascii_lowercase() == lower)
        .map(|(_, value)| value.as_str())
}

/// 头名大小写无关插入：同名（不同大小写）只保留一个，不重复发送。
pub fn insert_ci(map: &mut HashMap<String, String>, name: &str, value: String) {
    let lower = name.to_ascii_lowercase();
    map.retain(|key, _| key.to_ascii_lowercase() != lower);
    map.insert(canonical_name(name), value);
}

/// 移除（大小写无关）。
pub fn remove_ci(map: &mut HashMap<String, String>, name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let key = map
        .keys()
        .find(|key| key.to_ascii_lowercase() == lower)
        .cloned();
    key.and_then(|key| map.remove(&key))
}

/// 按优先级计算一次请求要发的头：
///
/// 1. 用户显式（合法空值 = 不发送该头）
/// 2. 解析服务明确下发
/// 3. 当前请求目标 URL 的平台规则
/// 4. 原内容页规则 / origin
/// 5. 不带该头
///
/// User-Agent 始终有默认值（Chrome131，与 wreq 指纹同大版本）。
pub fn resolve_request_headers(
    sources: &HeaderSources,
    target: &HeaderTarget<'_>,
) -> ResolvedHeaders {
    let mut resolved = ResolvedHeaders::default();

    // User-Agent：默认 Chrome131，显式可覆盖。
    resolved.sent.insert(
        "User-Agent".to_string(),
        DEFAULT_DOWNLOAD_USER_AGENT.to_string(),
    );

    // 显式层：空值表示明确不发送，记进 suppressed 并挡掉后面的自动推断。
    for (name, value) in &sources.explicit {
        if !valid_header_name(name) {
            continue;
        }
        if value.is_empty() {
            resolved.suppressed.push(canonical_name(name));
            continue;
        }
        if !valid_header_value(value) {
            continue;
        }
        insert_ci(&mut resolved.sent, name, value.clone());
    }
    for name in &resolved.suppressed {
        remove_ci(&mut resolved.sent, name);
    }

    // 解析下发层：不覆盖显式，也不覆盖已被显式空值压制的头。
    for (name, value) in &sources.resolver {
        if !valid_header_name(name) || !valid_header_value(value) || value.is_empty() {
            continue;
        }
        let canonical = canonical_name(name);
        if resolved.suppressed.iter().any(|s| s == &canonical) {
            continue;
        }
        if sources.explicit_get(name).is_some() {
            continue;
        }
        insert_ci(&mut resolved.sent, name, value.clone());
    }

    // Referer 单独走优先级链（显式 > 解析下发 > 目标 URL 规则 > 页面 origin > 不发）。
    if !resolved.suppressed.iter().any(|s| s == "Referer")
        && sources.explicit_get("Referer").is_none()
        && sources.resolver_get("Referer").is_none()
    {
        let auto = referer_for_urls(&[Some(target.url)])
            .or_else(|| referer_for_urls(&[target.page_url]))
            .or_else(|| origin_of(target.page_url));
        if let Some(referer) = auto {
            insert_ci(&mut resolved.sent, "Referer", referer);
        }
    }

    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ua_is_recent_chrome() {
        assert!(DEFAULT_DOWNLOAD_USER_AGENT.contains("Chrome/131"));
    }

    #[test]
    fn referer_matrix_covers_platforms() {
        let cases: &[(&str, &str)] = &[
            (
                "https://upos-sz-mirror.bilivideo.com/x.mp4",
                "https://www.bilibili.com/",
            ),
            (
                "https://www.bilibili.com/video/BV1.mp4",
                "https://www.bilibili.com/",
            ),
            (
                "https://tx-safety.kwaicdn.com/y.mp4",
                "https://www.kuaishou.com/",
            ),
            (
                "https://v3-web.toutiaovod.com/z.mp4",
                "https://www.toutiao.com/",
            ),
            (
                "https://v9-x.ixiguavideo.com/z.mp4",
                "https://www.toutiao.com/",
            ),
            (
                "https://v26-web.douyinvod.com/a.mp4",
                "https://www.douyin.com/",
            ),
            (
                "https://sns-video-hw.xhscdn.com/b.mp4",
                "https://www.xiaohongshu.com/",
            ),
        ];
        for (url, referer) in cases {
            assert_eq!(
                referer_for_urls(&[Some(url)]).as_deref(),
                Some(*referer),
                "{}",
                url
            );
        }
    }

    #[test]
    fn unknown_host_falls_back_to_page_origin() {
        let headers = default_download_headers(
            &[Some("https://cdn.example.com/a.mp4")],
            Some("https://site.example.com/video/1"),
        );
        assert_eq!(
            headers.get("Referer").map(String::as_str),
            Some("https://site.example.com")
        );
        let headers = default_download_headers(&[Some("https://cdn.example.com/a.mp4")], None);
        assert_eq!(headers.get("Referer"), None);
        assert!(headers.contains_key("User-Agent"));
    }

    #[test]
    fn notbilibili_lookalike_is_not_matched() {
        assert_eq!(
            referer_for_urls(&[Some("https://notbilibili.com/a.mp4")]),
            None
        );
    }

    #[test]
    fn provided_headers_win_over_defaults() {
        let mut provided = HashMap::new();
        provided.insert(
            "Referer".to_string(),
            "https://override.example/".to_string(),
        );
        provided.insert("X-Token".to_string(), "t".to_string());
        let merged = merge_download_headers(
            Some(&provided),
            &[Some("https://v26-web.douyinvod.com/a.mp4")],
            None,
        );
        assert_eq!(
            merged.get("Referer").map(String::as_str),
            Some("https://override.example/")
        );
        assert_eq!(merged.get("X-Token").map(String::as_str), Some("t"));
        assert_eq!(
            merged.get("User-Agent").map(String::as_str),
            Some(DEFAULT_DOWNLOAD_USER_AGENT)
        );
    }

    fn sources(explicit: &[(&str, &str)], resolver: &[(&str, &str)]) -> HeaderSources {
        HeaderSources {
            explicit: explicit
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            resolver: resolver
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn referer优先级用户显式最高() {
        let resolved = resolve_request_headers(
            &sources(
                &[("Referer", "https://user.example/")],
                &[("Referer", "https://resolver.example/")],
            ),
            &HeaderTarget {
                url: "https://v26-web.douyinvod.com/a.mp4",
                page_url: Some("https://www.douyin.com/video/1"),
            },
        );
        assert_eq!(
            resolved.sent.get("Referer").map(String::as_str),
            Some("https://user.example/"),
            "用户显式 Referer 最高优先"
        );
    }

    #[test]
    fn referer优先级解析下发次之() {
        let resolved = resolve_request_headers(
            &sources(&[], &[("Referer", "https://resolver.example/")]),
            &HeaderTarget {
                url: "https://v26-web.douyinvod.com/a.mp4",
                page_url: Some("https://www.douyin.com/video/1"),
            },
        );
        assert_eq!(
            resolved.sent.get("Referer").map(String::as_str),
            Some("https://resolver.example/")
        );
    }

    #[test]
    fn referer优先级目标url平台规则再次之() {
        let resolved = resolve_request_headers(
            &sources(&[], &[]),
            &HeaderTarget {
                url: "https://upos-sz-mirror.bilivideo.com/x.mp4",
                page_url: Some("https://www.douyin.com/video/1"),
            },
        );
        assert_eq!(
            resolved.sent.get("Referer").map(String::as_str),
            Some("https://www.bilibili.com/"),
            "目标 URL 是 B 站 CDN 就用 B 站规则，不用抖音页面"
        );
    }

    #[test]
    fn referer优先级页面origin兜底未知域() {
        let resolved = resolve_request_headers(
            &sources(&[], &[]),
            &HeaderTarget {
                url: "https://cdn.example.com/a.mp4",
                page_url: Some("https://site.example.com/video/1"),
            },
        );
        assert_eq!(
            resolved.sent.get("Referer").map(String::as_str),
            Some("https://site.example.com"),
            "未知域退回页面 origin，不硬塞抖音/头条"
        );
    }

    #[test]
    fn referer优先级都没有就不发() {
        let resolved = resolve_request_headers(
            &sources(&[], &[]),
            &HeaderTarget {
                url: "https://cdn.example.com/a.mp4",
                page_url: None,
            },
        );
        assert!(
            !resolved.sent.contains_key("Referer"),
            "推断不出来就不带 Referer，不瞎猜"
        );
        assert!(resolved.sent.contains_key("User-Agent"), "UA 始终有默认值");
    }

    #[test]
    fn 显式空referer表示不发送且压制自动推断() {
        let resolved = resolve_request_headers(
            &sources(
                &[("Referer", "")],
                &[("Referer", "https://resolver.example/")],
            ),
            &HeaderTarget {
                url: "https://v26-web.douyinvod.com/a.mp4",
                page_url: Some("https://www.douyin.com/video/1"),
            },
        );
        assert!(
            !resolved.sent.contains_key("Referer"),
            "合法空值表达「不要发 Referer」"
        );
        assert!(resolved.suppressed.iter().any(|name| name == "Referer"));
    }

    #[test]
    fn 头名大小写无关合并不重复发送() {
        let mut provided = HashMap::new();
        provided.insert("REFERER".to_string(), "https://a.example/".to_string());
        provided.insert("referer".to_string(), "https://b.example/".to_string());
        let merged = merge_download_headers(
            Some(&provided),
            &[Some("https://v26-web.douyinvod.com/a.mp4")],
            None,
        );
        let referer = get_ci(&merged, "Referer").expect("要有一个 Referer");
        assert!(
            referer == "https://a.example/" || referer == "https://b.example/",
            "同名头只留一个值: {referer}"
        );
        assert_eq!(
            merged
                .keys()
                .filter(|k| k.eq_ignore_ascii_case("referer"))
                .count(),
            1,
            "REFERER 与 Referer 不得各发一次"
        );
    }

    #[test]
    fn 自动推断头不冒充用户头() {
        // 解析服务下发 Some(空表)：只应得到自动推断，不能被当成用户显式锁死候选。
        let resolved = resolve_request_headers(
            &sources(&[], &[]),
            &HeaderTarget {
                url: "https://v26-web.douyinvod.com/a.mp4",
                page_url: None,
            },
        );
        assert_eq!(
            resolved.sent.get("Referer").map(String::as_str),
            Some("https://www.douyin.com/"),
            "只有自动推断，来源是 Auto 不是 Explicit"
        );
    }

    #[test]
    fn 非法头被丢弃绝不发出去() {
        let resolved = resolve_request_headers(
            &sources(
                &[("X-Bad\r\nInject", "v"), ("X-Ok", "line1\nline2")],
                &[("X-Also-Bad", "a\rb")],
            ),
            &HeaderTarget {
                url: "https://cdn.example.com/a.mp4",
                page_url: None,
            },
        );
        assert!(
            resolved.sent.keys().all(|name| valid_header_name(name)),
            "头名必须合法: {:?}",
            resolved.sent
        );
        assert!(
            resolved
                .sent
                .values()
                .all(|value| valid_header_value(value)),
            "头值不得含换行: {:?}",
            resolved.sent
        );
        assert!(get_ci(&resolved.sent, "X-Ok").is_none());
        assert!(get_ci(&resolved.sent, "X-Also-Bad").is_none());
    }

    #[test]
    fn 静态头只含与目标url无关的层() {
        // 每跳重算自动头时的基座：显式 + 解析下发 + 默认 UA，但**绝不含**按初始 URL
        // 推断出来的 Referer（否则重定向到别的平台时旧自动 Referer 会锁死新目标规则）。
        let layered = sources(
            &[("X-Token", "t")],
            &[("Referer", "https://resolver.example/")],
        );
        let flat = layered.static_headers();
        assert_eq!(
            flat.get("Referer").map(String::as_str),
            Some("https://resolver.example/"),
            "解析下发的 Referer 属于与 URL 无关的静态层"
        );
        assert_eq!(flat.get("X-Token").map(String::as_str), Some("t"));
        assert_eq!(
            flat.get("User-Agent").map(String::as_str),
            Some(DEFAULT_DOWNLOAD_USER_AGENT),
            "UA 与目标 URL 无关，静态层直接给默认值"
        );

        let auto_only = sources(&[], &[]).static_headers();
        assert!(
            !auto_only.contains_key("Referer"),
            "静态层不得包含任何自动 Referer: {auto_only:?}"
        );
    }

    #[test]
    fn 静态头保留显式空值作为压制标记() {
        let layered = sources(
            &[("Referer", "")],
            &[("Referer", "https://resolver.example/")],
        );
        let flat = layered.static_headers();
        assert_eq!(
            flat.get("Referer").map(String::as_str),
            Some(""),
            "显式空值必须保留为空串（压制标记），不能被解析下发层顶回来"
        );
    }

    #[test]
    fn 每个候选各自算referer() {
        // 备用候选换平台时要按各自目标 URL 重算，不能锁死第一候选的自动 Referer。
        let first = resolve_request_headers(
            &sources(&[], &[]),
            &HeaderTarget {
                url: "https://v26-web.douyinvod.com/a.mp4",
                page_url: None,
            },
        );
        let backup = resolve_request_headers(
            &sources(&[], &[]),
            &HeaderTarget {
                url: "https://upos-sz-mirror.bilivideo.com/b.mp4",
                page_url: None,
            },
        );
        assert_eq!(
            first.sent.get("Referer").map(String::as_str),
            Some("https://www.douyin.com/")
        );
        assert_eq!(
            backup.sent.get("Referer").map(String::as_str),
            Some("https://www.bilibili.com/")
        );
    }
}
