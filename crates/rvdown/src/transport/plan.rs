//! Range 分段规划：最小分段大小沿用 aria2 的 `--min-split-size` 语义，
//! 视频下载再按文件大小与来源自适应收窄调用者给定的连接上限。
//!
//! 纯函数、无 IO：编排层（segment.rs）拿它规划 N 路 Range 请求。

/// 默认并行连接数（对齐 aria2 `--max-connection-per-server`/`--split` 的默认值 8）。
pub const DEFAULT_SEGMENT_CONNECTIONS: usize = 8;

/// 切分下限（aria2 `--min-split-size=1M`）：每段至少这么大才值得单独开连接。
pub const MIN_SPLIT_SIZE_BYTES: u64 = 1024 * 1024;

/// 连接数夹取范围的上限（与 DOWNLOAD_CONCURRENCY 的口径一致，防误配打爆 CDN）。
pub const MAX_SEGMENT_CONNECTIONS: usize = 32;

/// 抖音资源超过该大小才启用多路；来自五平台文件测速的候选分界。
const ADAPTIVE_MULTI_CONNECTION_SIZE_BYTES: u64 = 32 * 1024 * 1024;

/// 单段区间：字节偏移 `[start, end]`（两端都含）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRange {
    pub index: usize,
    pub start: u64,
    /// 含端点：段的最后一个字节偏移。
    pub end: u64,
}

impl SegmentRange {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    /// 段长恒 ≥ 1 字节（规划器不产出空段），保留给 clippy 语义一致性。
    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }

    /// HTTP Range 头值：`bytes=0-1048575`。
    pub fn header_value(&self) -> String {
        format!("bytes={}-{}", self.start, self.end)
    }
}

/// 把用户/环境给的连接数收敛到 1..=32；0 或缺失回落默认 8。
pub fn normalize_connections(raw: Option<usize>) -> usize {
    let value = raw.unwrap_or(DEFAULT_SEGMENT_CONNECTIONS);
    if value == 0 {
        return DEFAULT_SEGMENT_CONNECTIONS;
    }
    value.min(MAX_SEGMENT_CONNECTIONS)
}

/// 按文件大小/来源收窄连接上限。`requested` 是调用者配置的最大值，不会被提高。
///
/// 只对抖音实施 ≤32MiB 单路：五平台同轮测试显示抖音稳定受益，头条小文件则有回退。
pub(crate) fn adaptive_connection_limit(total: u64, requested: usize, is_douyin: bool) -> usize {
    let requested = normalize_connections(Some(requested));
    if is_douyin && total <= ADAPTIVE_MULTI_CONNECTION_SIZE_BYTES {
        1
    } else {
        requested
    }
}

/// aria2 口径的段数：连接数受「每段至少 min_split_size」约束打折——
/// `n = min(split, total / min_split)`（整除向下取整，至少 1 段）。
/// total=2.5M 时是 2 段（每段 1.25M），不是 3 段 0.83M——低于下限的切片不值得开连接。
pub fn segment_count(total: u64, connections: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let by_min_split = (total / MIN_SPLIT_SIZE_BYTES) as usize;
    if by_min_split == 0 {
        return 1;
    }
    by_min_split.min(connections).max(1)
}

/// 把 total 均分为 n 段：前面的段吃余数，段与段首尾相接、不重不漏。
/// total 为 0 返回空（调用方走单流）。
pub fn plan_segments(total: u64, connections: usize) -> Vec<SegmentRange> {
    let n = segment_count(total, connections);
    if n == 0 {
        return Vec::new();
    }
    let base = total / n as u64;
    let remainder = total % n as u64;
    let mut segments = Vec::with_capacity(n);
    let mut offset = 0u64;
    for index in 0..n {
        let len = base + if (index as u64) < remainder { 1 } else { 0 };
        segments.push(SegmentRange {
            index,
            start: offset,
            end: offset + len - 1,
        });
        offset += len;
    }
    debug_assert_eq!(offset, total, "分段必须精确覆盖整个文件");
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u64 = MIN_SPLIT_SIZE_BYTES;

    #[test]
    fn connections_normalize_with_clamp_and_default() {
        assert_eq!(normalize_connections(None), DEFAULT_SEGMENT_CONNECTIONS);
        assert_eq!(normalize_connections(Some(0)), DEFAULT_SEGMENT_CONNECTIONS);
        assert_eq!(normalize_connections(Some(1)), 1);
        assert_eq!(normalize_connections(Some(64)), MAX_SEGMENT_CONNECTIONS);
    }

    #[test]
    fn zero_total_means_no_segments() {
        assert!(plan_segments(0, 8).is_empty());
        assert_eq!(segment_count(0, 8), 0);
    }

    #[test]
    fn tiny_file_stays_single_segment() {
        // 小于 min-split-size 的文件只开 1 条连接（aria2 同样不打折以下的片）。
        let segments = plan_segments(500 * 1024, 8);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start, 0);
        assert_eq!(segments[0].end, 500 * 1024 - 1);
        assert_eq!(segments[0].header_value(), "bytes=0-511999");
    }

    #[test]
    fn one_and_half_meg_stays_single_but_two_and_half_splits_in_two() {
        // 1.5M：整除 1M 得 1，两片 0.75M 低于下限，不开连接（aria2 同口径）。
        assert_eq!(plan_segments(M + M / 2, 8).len(), 1);
        // 2.5M：整除 1M 得 2 —— 每段 1.25M，而不是 3 段 0.83M。
        let segments = plan_segments(2 * M + M / 2, 8);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].len(), M + M / 4);
        assert_eq!(segments[1].len(), M + M / 4);
        assert!(segments.iter().all(|s| s.len() >= MIN_SPLIT_SIZE_BYTES));
    }

    #[test]
    fn big_file_caps_at_connections_with_even_split() {
        let segments = plan_segments(100 * M, 8);
        assert_eq!(segments.len(), 8);
        // 100M 均分 8 段：4 段 12.5M（100M/8 整除）。
        assert!(segments.iter().all(|s| s.len() == 12 * M + M / 2));
        assert_eq!(segments[0].start, 0);
        assert_eq!(segments[7].end, 100 * M - 1);
    }

    #[test]
    fn adaptive_connections_limit_only_douyin_small_files() {
        const THIRTY_TWO_MIB: u64 = 32 * M;

        assert_eq!(adaptive_connection_limit(3 * M, 8, true), 1);
        assert_eq!(adaptive_connection_limit(THIRTY_TWO_MIB, 8, true), 1);
        assert_eq!(adaptive_connection_limit(THIRTY_TWO_MIB + 1, 8, true), 8);
        assert_eq!(adaptive_connection_limit(5 * M, 8, false), 8);
        assert_eq!(adaptive_connection_limit(20 * M, 8, false), 8);
        assert_eq!(adaptive_connection_limit(100 * M, 2, true), 2);
        assert_eq!(adaptive_connection_limit(100 * M, 64, true), 32);
        assert_eq!(adaptive_connection_limit(100 * M, 0, true), 8);
    }

    #[test]
    fn odd_size_distributes_remainder_to_front_segments() {
        // 10M + 1 字节分 8 段：前 1 段多吃 1 字节，覆盖不重不漏。
        let total = 10 * M + 1;
        let segments = plan_segments(total, 8);
        assert_eq!(segments.len(), 8);
        let mut cursor = 0u64;
        for (i, s) in segments.iter().enumerate() {
            assert_eq!(s.start, cursor);
            assert_eq!(s.end + 1 - s.start, s.len());
            cursor = s.end + 1;
            assert_eq!(s.index, i);
        }
        assert_eq!(cursor, total, "分段必须精确覆盖整个文件");
    }

    #[test]
    fn connections_of_one_yields_single_segment() {
        let segments = plan_segments(50 * M, 1);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].end, 50 * M - 1);
    }
}
