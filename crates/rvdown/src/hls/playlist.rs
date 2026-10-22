//! HLS 播放列表解析：master / media 分型、清晰度选择、加密明确失败。
//!
//! URL 后缀不可信，快手 m3u8 常不带 `.m3u8` 后缀；解析服务有时把成片给成 m3u8，
//! aria2 会把播放列表本身当文件下完——实测 16KB。因此按内容判断，不看 URL。
//!
//! **绝不把 playlist 当成品交付**；加密流明确失败，不静默降级。

/// 播放列表起始标记。
pub const PLAYLIST_MARK: &str = "#EXTM3U";

/// HLS 解析失败分类（用户可见话术固定）。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HlsError {
    #[error("不是有效的播放列表")]
    NotAPlaylist,
    #[error("播放列表已加密，无法直接下载")]
    Encrypted,
    #[error("播放列表没有分片")]
    NoSegments,
    #[error("播放列表没有可用清晰度")]
    NoVariants,
}

/// master 清单的一个清晰度候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub bandwidth: u64,
    pub url: String,
}

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playlist {
    /// `#EXT-X-STREAM-INF` 清单：给多个清晰度，选最高的再取 media 清单。
    Master { variants: Vec<Variant> },
    /// 分片清单：`segments` 是**绝对 URL**。
    Media { segments: Vec<String> },
}

/// 是否是 HLS 播放列表（按内容判断，不看 URL 后缀）。
pub fn is_hls_content(text: &str) -> bool {
    text.trim_start().starts_with(PLAYLIST_MARK)
}

/// 解析播放列表；相对 URL 以 `base_url` 解析成绝对 URL。
pub fn parse_playlist(text: &str, base_url: &str) -> Result<Playlist, HlsError> {
    if !is_hls_content(text) {
        return Err(HlsError::NotAPlaylist);
    }
    // 加密流明确失败：`#EXT-X-KEY:METHOD=` 非 NONE 就没法直接下载。
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            // `METHOD=` 可能出现在标签名之后（`#EXT-X-KEY:METHOD=...`），不是每段的前缀。
            let method = rest
                .split(',')
                .find_map(|part| {
                    let part = part.trim();
                    let index = part.find("METHOD=")?;
                    Some(part[index + "METHOD=".len()..].trim())
                })
                .unwrap_or("NONE");
            if !method.eq_ignore_ascii_case("NONE") {
                return Err(HlsError::Encrypted);
            }
        }
    }

    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines
        .iter()
        .any(|line| line.starts_with("#EXT-X-STREAM-INF"))
    {
        let mut variants = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if !line.starts_with("#EXT-X-STREAM-INF") {
                continue;
            }
            let Some(next) = lines.get(index + 1) else {
                continue;
            };
            if next.starts_with('#') {
                continue;
            }
            // `BANDWIDTH=` 在 `#EXT-X-STREAM-INF:` 之后，不是逗号段的前缀。
            let bandwidth = line
                .split(',')
                .find_map(|part| {
                    let part = part.trim();
                    let index = part.find("BANDWIDTH=")?;
                    let value = part[index + "BANDWIDTH=".len()..].trim();
                    value.parse::<u64>().ok()
                })
                .unwrap_or(0);
            variants.push(Variant {
                bandwidth,
                url: join(base_url, next),
            });
        }
        if variants.is_empty() {
            return Err(HlsError::NoVariants);
        }
        return Ok(Playlist::Master { variants });
    }

    let segments: Vec<String> = lines
        .iter()
        .filter(|line| !line.starts_with('#'))
        .map(|line| join(base_url, line))
        .collect();
    if segments.is_empty() {
        return Err(HlsError::NoSegments);
    }
    Ok(Playlist::Media { segments })
}

/// 取最高带宽清晰度。
pub fn highest_bandwidth_variant(variants: &[Variant]) -> Result<&Variant, HlsError> {
    variants
        .iter()
        .max_by_key(|variant| variant.bandwidth)
        .ok_or(HlsError::NoVariants)
}

/// 相对 URL 解析到绝对 URL（base 缺 scheme 时原样返回，交由上层报错）。
fn join(base: &str, reference: &str) -> String {
    let Ok(base) = url::Url::parse(base) else {
        return reference.to_string();
    };
    base.join(reference)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| reference.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
        low/index.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\n\
        high/index.m3u8\n";

    const MEDIA: &str = "#EXTM3U\n\
        #EXT-X-TARGETDURATION:10\n\
        #EXTINF:9.9,\n\
        seg0.ts\n\
        #EXTINF:9.9,\n\
        https://cdn.test/seg1.ts\n\
        #EXT-X-ENDLIST\n";

    #[test]
    fn 按内容识别播放列表不看后缀() {
        // 快手 m3u8 常不带 .m3u8 后缀，只能按内容判断。
        assert!(is_hls_content("#EXTM3U\n#EXT-X-ENDLIST"));
        assert!(!is_hls_content("not a playlist"));
    }

    #[test]
    fn master清单选最高带宽() {
        let parsed = parse_playlist(MASTER, "https://cdn.test/hls/index.m3u8").unwrap();
        let Playlist::Master { variants } = parsed else {
            panic!("应是 master 清单")
        };
        assert_eq!(variants.len(), 2);
        assert_eq!(
            variants[0].url, "https://cdn.test/hls/low/index.m3u8",
            "相对 URL 按 base 解析"
        );
        let picked = highest_bandwidth_variant(&variants).unwrap();
        assert_eq!(picked.bandwidth, 2_400_000, "取最高清晰度");
    }

    #[test]
    fn media清单取分片绝对地址() {
        let parsed = parse_playlist(MEDIA, "https://cdn.test/hls/index.m3u8").unwrap();
        let Playlist::Media { segments } = parsed else {
            panic!("应是 media 清单")
        };
        assert_eq!(
            segments,
            vec![
                "https://cdn.test/hls/seg0.ts".to_string(),
                "https://cdn.test/seg1.ts".to_string()
            ]
        );
    }

    #[test]
    fn 加密播放列表明确失败() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n#EXTINF:9.9,\nseg0.ts\n";
        assert_eq!(
            parse_playlist(text, "https://c.test/a.m3u8"),
            Err(HlsError::Encrypted)
        );
    }

    #[test]
    fn 明确声明不加密是允许的() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:9.9,\nseg0.ts\n#EXT-X-ENDLIST\n";
        assert!(parse_playlist(text, "https://c.test/a.m3u8").is_ok());
    }

    #[test]
    fn 空清单与非清单明确失败() {
        assert_eq!(
            parse_playlist("hello", "https://c.test/a"),
            Err(HlsError::NotAPlaylist)
        );
        assert_eq!(
            parse_playlist("#EXTM3U\n#EXT-X-TARGETDURATION:10\n", "https://c.test/a"),
            Err(HlsError::NoSegments)
        );
    }
}
