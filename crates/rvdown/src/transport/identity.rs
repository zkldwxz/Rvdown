//! 资源身份与字节区间：断点续传的「这是不是同一份资源 / 哪些字节已经可信」。
//!
//! 残留残件必须从零重下、不把上次的坏文件拼进新下载；`.aria2` 控制文件语义沿用
//! aria2 的 `--continue` / `--file-allocation=none`。
//!
//! v1 控制文件只记 `total` + 段下标，有三个致命缺口：
//! 1. connections 一变段下标就错位；
//! 2. 同大小换内容识别不了；
//! 3. 签名 URL 刷新 / 备用 URL 切换不能按 URL 字符串认定同一资源。
//!
//! 因此身份 = **强验证器 + 长度 + 资源域**，进度只认**字节区间**。
//!
//! 资源域（RFC 9110 §8.8.1：不同资源的强 validator 不保证全局唯一——两个 URL 各自
//! 返回 `"v1"` + 同 size 很常见）是完整请求 URL 的不可逆摘要：跨 URL（签名刷新 /
//! 备用候选 / 换 CDN）在不能证明等价时一律按不同资源处理、安全重下；同 URL 字符串
//! 才谈得上续传。Last-Modified 只作诊断信息，不参与任何确认判定（§13.1.5：HTTP-date
//! 只有在没有 etag 且日期可证强时才能用，客户端拿不到 Date 证据）。

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

/// 资源域：完整请求 URL（含 query）的 MD5 十六进制摘要。
///
/// 为什么不去 query / 不做归一化：签名刷新、跨候选切换时无法证明新旧 URL 指向同一
/// 字节，宁可重下也不拼旧段；MD5 只当防误拼的域标签（不是安全签名），沿用本 crate
/// 已有依赖，不为此新增哈希库。
pub fn resource_domain(url: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(url.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 资源身份：验证器与长度（URL 维度由台账的 `origin` 资源域把关，见 `resource_domain`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceIdentity {
    /// ETag 原样（含 `W/` 前缀）；None 表示服务器没给。
    pub etag: Option<String>,
    /// `Last-Modified` 的 HTTP-date 原文；None 表示服务器没给。
    pub last_modified: Option<String>,
    /// 资源总长度（与验证器一起构成身份）。
    pub length: u64,
}

impl ResourceIdentity {
    pub fn new(etag: Option<String>, last_modified: Option<String>, length: u64) -> Self {
        Self {
            etag,
            last_modified,
            length,
        }
    }

    /// 只有**语法合法的强 ETag** 是可确认的强验证器。
    ///
    /// 弱 ETag（`W/"..."`）按 RFC 9110 只保证语义等价、不保证字节一致，
    /// **不能做 If-Range**；Last-Modified 是 HTTP-date，没有 Date 证据无法证明
    /// 「日期之后没变过」，只作诊断，不参与确认。
    ///
    /// 语法不合法的值（空串、缺引号、含空格/控制字符、大小写弱前缀）一律不算强验证器：
    /// 把它们当强 tag 会让一次格式错误的上游响应污染整个续传判定。
    pub fn strong_etag(&self) -> Option<&str> {
        self.etag
            .as_deref()
            .filter(|value| is_strong_entity_tag(value))
    }

    /// `If-Range` 头的值：**只允许强 ETag**（RFC 9110 §13.1.5）。
    ///
    /// Last-Modified 不再用作 If-Range：HTTP-date 只有在没有 etag 且日期可证强时
    /// 才合法，客户端没有 Date 证据，LM 一律只当诊断信息。
    pub fn if_range_value(&self) -> Option<&str> {
        self.strong_etag()
    }

    /// 与台账里记的旧身份比对，确认「同一资源」才能续传。
    ///
    /// 只认**两边都有且相等的强 ETag**（外加长度相等）；Last-Modified 相等证明不了
    /// 字节一致，不参与确认。无法确认（验证器缺失 / 只有弱 ETag / 只有 LM /
    /// 长度变了）一律返回 false → 安全重下，宁可多下一次也不把旧字节拼进新文件。
    pub fn confirms_same_resource(&self, previous: &Self) -> bool {
        if self.length != previous.length {
            return false;
        }
        matches!(
            (self.strong_etag(), previous.strong_etag()),
            (Some(current), Some(old)) if current == old
        )
    }

    /// 台账里有没有可确认身份的强验证器（Last-Modified 只算诊断，不算数）。
    pub fn has_usable_validator(&self) -> bool {
        self.strong_etag().is_some()
    }

    /// 下载过程中的验证器比对（**不含长度**）：两边都有强 ETag 时不一致才判资源变化；
    /// 有一边核不了就无从判断，不误判（Last-Modified 只诊断，不参与判定）。
    ///
    /// 与 `confirms_same_resource` 的区别：那条用于续传判定（长度也是身份的一部分）；
    /// 这条用于传输途中——单流响应的 Content-Length 可能只是本次响应的长度，
    /// 拿它当资源长度会把「空响应」误判成「换资源」。
    pub fn confirms_same_validators(&self, previous: &Self) -> bool {
        match (self.strong_etag(), previous.strong_etag()) {
            (Some(current), Some(old)) => current == old,
            _ => true,
        }
    }
}

/// 是否是**强** entity-tag（RFC 9110 §8.8.3）：
/// `entity-tag = [ "W/" ] DQUOTE *etagc DQUOTE`，`etagc = %x21 / %x23-7E / obs-text`。
///
/// 另加两条保守判定：
/// - 小写 `w/` 前缀也按弱处理（语法上只有大写 `W/` 合法，宁可当弱也不当强）；
/// - 不 trim、不修正引号：格式不对就是格式不对，交给上层重下而不是猜。
pub fn is_strong_entity_tag(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    if bytes[0] != b'"' || bytes[bytes.len() - 1] != b'"' {
        return false;
    }
    bytes[1..bytes.len() - 1]
        .iter()
        .all(|byte| matches!(byte, 0x21 | 0x23..=0x7E | 0x80..=0xFF))
}

/// 闭区间字节范围 `[start, end]`（两端都含）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// 区间长度；`end < start` 时空区间。
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }

    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }

    /// 是否与另一个区间重叠（闭区间，首尾相接不算重叠）。
    pub fn overlaps(&self, other: &Self) -> bool {
        !self.is_empty() && !other.is_empty() && self.start <= other.end && other.start <= self.end
    }

    /// 是否完全包含另一个区间。
    pub fn contains_range(&self, other: &Self) -> bool {
        !other.is_empty() && self.start <= other.start && other.end <= self.end
    }
}

/// 归一化区间集合：去空、升序、合并重叠与相邻，越界区间裁到 `[0, total]` 内。
pub fn normalize_ranges(ranges: &[ByteRange], total: u64) -> Vec<ByteRange> {
    let mut cleaned: Vec<ByteRange> = ranges
        .iter()
        .filter(|range| !range.is_empty())
        .map(|range| ByteRange {
            start: range.start.min(total.saturating_sub(1)),
            end: range.end.min(total.saturating_sub(1)),
        })
        .filter(|range| !range.is_empty())
        .collect();
    cleaned.sort();
    let mut merged: Vec<ByteRange> = Vec::with_capacity(cleaned.len());
    for range in cleaned {
        match merged.last_mut() {
            // 相邻（end+1 == start）也算连续，合并掉。
            Some(last) if range.start <= last.end.saturating_add(1) => {
                last.end = last.end.max(range.end);
            }
            _ => merged.push(range),
        }
    }
    merged
}

/// 区间减法：`target` 去掉 `done` 已覆盖的部分，返回待取子区间（升序、不相交）。
pub fn subtract(target: ByteRange, done: &[ByteRange]) -> Vec<ByteRange> {
    if target.is_empty() {
        return Vec::new();
    }
    let mut missing = vec![target];
    for covered in normalize_ranges(done, u64::MAX) {
        if covered.is_empty() {
            continue;
        }
        let mut next = Vec::with_capacity(missing.len() + 1);
        for piece in missing {
            if !piece.overlaps(&covered) {
                next.push(piece);
                continue;
            }
            if piece.start < covered.start {
                next.push(ByteRange::new(piece.start, covered.start - 1));
            }
            if covered.end < piece.end {
                next.push(ByteRange::new(covered.end + 1, piece.end));
            }
        }
        missing = next;
    }
    missing
}

/// `done` 是否完整覆盖 `target`。
pub fn covers(done: &[ByteRange], target: ByteRange) -> bool {
    if target.is_empty() {
        return true;
    }
    subtract(target, done).is_empty()
}

/// `Content-Range` 响应头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRange {
    /// `bytes 0-1023/2048` 或 `bytes 0-1023/*`。
    Satisfied {
        start: u64,
        end: u64,
        total: Option<u64>,
    },
    /// `bytes */2048`：请求区间不可满足（416 的标准形态）。
    Unsatisfied { total: Option<u64> },
}

impl ContentRange {
    /// 解析 `Content-Range` 头；格式不对返回 None（由调用方按缺失处理）。
    pub fn parse(value: &str) -> Option<Self> {
        let rest = value.trim().strip_prefix("bytes")?.trim();
        let (range_part, total_part) = rest.split_once('/').map(|(a, b)| (a.trim(), b.trim()))?;
        let total = if total_part == "*" {
            None
        } else {
            Some(total_part.parse::<u64>().ok()?)
        };
        if range_part == "*" {
            return Some(Self::Unsatisfied { total });
        }
        let (start, end) = range_part.split_once('-')?;
        let start = start.trim().parse::<u64>().ok()?;
        let end = end.trim().parse::<u64>().ok()?;
        if end < start {
            return None;
        }
        Some(Self::Satisfied { start, end, total })
    }

    pub fn total(&self) -> Option<u64> {
        match self {
            Self::Satisfied { total, .. } | Self::Unsatisfied { total } => *total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 弱etag不能做if_range也不能确认资源() {
        let weak = ResourceIdentity::new(Some(r#"W/""abc""#.into()), None, 100);
        assert_eq!(weak.if_range_value(), None, "弱 ETag 不做 If-Range");
        assert!(
            !weak.has_usable_validator(),
            "弱 ETag 单独存在时无法确认资源身份"
        );
        assert!(
            !weak.confirms_same_resource(&weak.clone()),
            "只有弱 ETag 的台账绝不确认同一资源，宁可重下"
        );
    }

    #[test]
    fn 非法etag形状一律不算强验证器() {
        // RFC 9110 §8.8.3 entity-tag = [ "W/" ] DQUOTE *etagc DQUOTE，
        // etagc = %x21 / %x23-7E / obs-text。不在这个语法里的值（空串、缺引号、
        // 含空格/制表符/控制字符）一律不能当强验证器做 If-Range 或续传确认。
        let illegal = [
            "",             // 空字符串
            "v1",           // 没有 DQUOTE
            "\"v1",         // 缺右引号
            "v1\"",         // 缺左引号
            "\"v 1\"",      // 含空格（0x20 不在 etagc）
            "\"v1\t\"",     // 含制表符
            "\"v1\u{7f}\"", // DEL 不在 etagc（%x23-7E 到此为止）
            "\"v1\r\"",     // CR
            "W/\"v1\"",     // 弱标签
            "w/\"v1\"",     // 小写弱标签
        ];
        for raw in illegal {
            let identity = ResourceIdentity::new(Some(raw.to_string()), None, 100);
            assert_eq!(
                identity.strong_etag(),
                None,
                "非法 entity-tag 不能算强验证器: {raw:?}"
            );
            assert_eq!(
                identity.if_range_value(),
                None,
                "也不能做 If-Range: {raw:?}"
            );
            assert!(!identity.has_usable_validator(), "{raw:?}");
            assert!(
                !identity.clone().confirms_same_resource(&identity.clone()),
                "自己跟自己也不能凭非法 ETag 确认: {raw:?}"
            );
        }

        // 合法形态：普通值、空 opaque（grammar 允许）、etagc 边界字符。
        for raw in [r#""v1""#, r#""""#, "\"a!#~\"", "\"\u{80}\""] {
            let identity = ResourceIdentity::new(Some(raw.to_string()), None, 100);
            assert_eq!(
                identity.strong_etag(),
                Some(raw),
                "合法强 entity-tag 必须认: {raw:?}"
            );
        }
    }

    #[test]
    fn 强etag相等且长度相等才算同一资源() {
        let old = ResourceIdentity::new(Some(r#""abc""#.into()), None, 100);
        let same = ResourceIdentity::new(Some(r#""abc""#.into()), None, 100);
        assert!(same.confirms_same_resource(&old));
        assert_eq!(same.if_range_value(), Some(r#""abc""#));

        let changed = ResourceIdentity::new(Some(r#""xyz""#.into()), None, 100);
        assert!(
            !changed.confirms_same_resource(&old),
            "同大小换内容必须识别出来，不能把旧字节拼进新文件"
        );

        let resized = ResourceIdentity::new(Some(r#""abc""#.into()), None, 101);
        assert!(
            !resized.confirms_same_resource(&old),
            "长度变了就不是同一资源"
        );
    }

    #[test]
    fn 弱etag加last_modified也不能确认更不能做if_range() {
        // RFC 9110 §13.1.5：If-Range 只能带强验证器；HTTP-date 只有在「没有 etag 且
        // 日期可证强」时才可用——客户端没有 Date 证据，LM 一律只当诊断信息。
        let old = ResourceIdentity::new(None, Some("Wed, 21 Oct 2026 07:28:00 GMT".into()), 4096);
        let same = ResourceIdentity::new(
            Some(r#"W/""weak""#.into()),
            Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
            4096,
        );
        assert_eq!(same.if_range_value(), None, "LM 不再作为 If-Range 值");
        assert!(!same.has_usable_validator(), "LM 不算可确认的验证器");
        assert!(
            !same.confirms_same_resource(&old),
            "LM 相等只说明『可能没变过』，证明不了字节一致，续传确认只认强 etag"
        );
    }

    #[test]
    fn 无验证器一律无法确认() {
        let none = ResourceIdentity::new(None, None, 4096);
        assert!(!none.has_usable_validator());
        assert!(!none.confirms_same_resource(&none.clone()));
    }

    #[test]
    fn 资源域摘要不去query且不同url必不同() {
        // RFC 9110 §8.8.1：不同资源的强 validator 不保证全局唯一（各自 "v1" 很常见），
        // 台账必须另记资源域。保守口径：完整 URL（含 query）原样进摘要，
        // 跨签名刷新/跨候选不能证明等价就重下，不撞自作聪明归一化的坑。
        let a = resource_domain("https://host-a.test/v.mp4?sig=abc");
        let same_a = resource_domain("https://host-a.test/v.mp4?sig=abc");
        let b = resource_domain("https://host-b.test/v.mp4?sig=abc");
        let requery = resource_domain("https://host-a.test/v.mp4?sig=refreshed");
        assert_eq!(a, same_a, "同 URL 摘要稳定");
        assert_ne!(a, b, "不同 host 必不同域");
        assert_ne!(
            a, requery,
            "签名刷新（query 变）不能证明等价，按不同资源域处理"
        );
        assert_eq!(a.len(), 32, "摘要定长十六进制，便于台账排查");
    }

    #[test]
    fn 区间归一合并重叠与相邻() {
        let ranges = vec![
            ByteRange::new(10, 20),
            ByteRange::new(0, 5),
            ByteRange::new(21, 30), // 相邻，合并
            ByteRange::new(40, 30), // 空区间丢弃
            ByteRange::new(15, 18), // 重叠，合并
        ];
        let merged = normalize_ranges(&ranges, 100);
        assert_eq!(
            merged,
            vec![ByteRange::new(0, 5), ByteRange::new(10, 30)],
            "重叠与相邻区间必须合并，区间集合保持升序不相交"
        );
    }

    #[test]
    fn 区间减法算出待取字节() {
        let done = vec![ByteRange::new(0, 1310719), ByteRange::new(2621440, 3932159)];
        let target = ByteRange::new(0, 3932159);
        let missing = subtract(target, &done);
        assert_eq!(
            missing,
            vec![ByteRange::new(1310720, 2621439)],
            "连接数变化时按字节区间取差集，段下标错位不影响续传"
        );
        assert!(covers(&done, ByteRange::new(0, 1310719)));
        assert!(!covers(&done, target));
    }

    #[test]
    fn 同大小换内容在区间层面也能挡住() {
        // 旧台账记着 0..99 已完成；新资源同大小但内容不同 → 身份比对挡在前面，
        // 即便区间算得出来也不能续传。
        let old = ResourceIdentity::new(Some(r#""v1""#.into()), None, 100);
        let new = ResourceIdentity::new(Some(r#""v2""#.into()), None, 100);
        assert!(!new.confirms_same_resource(&old));
    }

    #[test]
    fn 途中验证器比对只看验证器不看长度() {
        // 单流响应的 Content-Length 可能只是本次响应长度，拿它当资源长度会误判「换资源」。
        let probed = ResourceIdentity::new(Some(r#""v1""#.into()), None, 100);
        let empty_body = ResourceIdentity::new(Some(r#""v1""#.into()), None, 0);
        assert!(
            empty_body.confirms_same_validators(&probed),
            "验证器一致就不能判资源变化"
        );
        let replaced = ResourceIdentity::new(Some(r#""v2""#.into()), None, 0);
        assert!(
            !replaced.confirms_same_validators(&probed),
            "换了资源要判出来"
        );
    }

    #[test]
    fn content_range解析满足与不满足形态() {
        assert_eq!(
            ContentRange::parse("bytes 0-1023/2048"),
            Some(ContentRange::Satisfied {
                start: 0,
                end: 1023,
                total: Some(2048)
            })
        );
        assert_eq!(
            ContentRange::parse("bytes 0-1023/*"),
            Some(ContentRange::Satisfied {
                start: 0,
                end: 1023,
                total: None
            })
        );
        assert_eq!(
            ContentRange::parse("bytes */2048"),
            Some(ContentRange::Unsatisfied { total: Some(2048) })
        );
        assert_eq!(ContentRange::parse("garbage"), None);
    }
}
