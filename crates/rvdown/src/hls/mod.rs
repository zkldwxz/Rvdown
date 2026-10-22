//! HLS（VOD）与 DASH 双流落地。
//!
//! HLS 侧负责清单解析、分片并发下载与 ffmpeg concat 封装。DASH 双流合并只有一份实现，
//! 在 `Engine`（两条流都下完才合并，缺音频硬失败）。

pub mod materialize;
pub mod playlist;

pub use materialize::{
    concat_list_line, normalize_hls_concurrency, segment_path, HlsFailure, HlsMaterializer,
    DEFAULT_HLS_CONCURRENCY, MAX_HLS_CONCURRENCY,
};
pub use playlist::{
    highest_bandwidth_variant, is_hls_content, parse_playlist, HlsError, Playlist, Variant,
};
