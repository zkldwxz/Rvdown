//! HLS 落地：拉清单 → 选清晰度 → 并发下分片 → ffmpeg concat 封装成 mp4。
//!
//! 流程：临时目录 → 分片命名 `0000.ts` → concat demuxer 封装 → 成功后 rename 到
//! dest、失败清场。
//!
//! **绝不把 playlist 当成品交付**：清单只当索引，产物必须是封装后的媒体文件。
//!
//! 落盘不变量：临时工作目录建在目标文件父目录（Windows TEMP 常在 C:、视频在 D:，
//! 跨卷 rename 必失败），所有中间文件与最终产物同卷；成功后 rename 交付，
//! **不提前删目标**——封装/校验/交付任一步失败，已有成片保持原样。

use std::path::{Path, PathBuf};

use futures::StreamExt;

use crate::error::DownloadError;
use crate::ffmpeg::{Ffmpeg, FfmpegError};
use crate::traits::FileDownloader;
use crate::transfer::budget::ConnectionBudget;

use super::playlist::{highest_bandwidth_variant, parse_playlist, HlsError, Playlist};

/// HLS 分片并发默认值（默认 16）。
pub const DEFAULT_HLS_CONCURRENCY: usize = 16;
/// HLS 分片并发上限（钳 1..=32）。
pub const MAX_HLS_CONCURRENCY: usize = 32;

/// HLS 落地失败分类（用户可见话术固定）。
#[derive(Debug, thiserror::Error)]
pub enum HlsFailure {
    #[error("{0}")]
    Parse(#[from] HlsError),
    #[error("下载失败: {0}")]
    Download(DownloadError),
    #[error("{0}")]
    Ffmpeg(#[from] FfmpegError),
    #[error("无法读取播放列表: {0}")]
    Io(String),
    #[error("封装产物为空，不能交付")]
    EmptyOutput,
}

/// HLS 落地器：只依赖 `FileDownloader` 与外置 ffmpeg，可注入可测试。
pub struct HlsMaterializer<'a> {
    pub downloader: &'a dyn FileDownloader,
    pub ffmpeg: &'a Ffmpeg,
    /// 请求头来源分槽：每个分片按各自 URL 重算 Referer。
    pub sources: &'a crate::headers::HeaderSources,
    pub page_url: Option<&'a str>,
    pub concurrency: usize,
    pub budget: Option<&'a ConnectionBudget>,
}

/// 把并发夹到 1..=32；0 或缺失回落默认 16。
pub fn normalize_hls_concurrency(raw: Option<usize>) -> usize {
    match raw {
        Some(value) if value > 0 => value.min(MAX_HLS_CONCURRENCY),
        _ => DEFAULT_HLS_CONCURRENCY,
    }
}

fn segment_name(index: usize) -> String {
    format!("{index:04}.ts")
}

/// 与目标文件同卷的本次工作目录；Drop 清理（含 future 取消/提前返回）。
///
/// 不能放系统临时目录：Windows 的 TEMP 常规在 C:，用户视频目录常在 D:，
/// 跨卷 rename 一定失败，而且失败前若已删目标就丢了旧成片。
struct WorkDir {
    path: PathBuf,
}
impl WorkDir {
    fn create_near(dest: &Path, kind: &str) -> Result<Self, std::io::Error> {
        static WORK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let parent = dest
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        // 目标父目录先建好：工作目录必须与最终产物同一文件系统。
        std::fs::create_dir_all(parent)?;
        for _ in 0..64 {
            let path = parent.join(format!(
                ".{kind}-{}-{}",
                std::process::id(),
                WORK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "无法创建临时工作目录",
        ))
    }
    fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 同卷原子交付：成功才 rename；失败时目标保持原样。
///
/// 不先 `remove_file(dest)`：Windows 的 `std::fs::rename` 走
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` 可覆盖文件，删旧文件反而在
/// rename 失败时直接丢掉已有成片。
fn commit(tmp: &Path, dest: &Path) -> Result<(), std::io::Error> {
    std::fs::rename(tmp, dest)
}

/// concat 清单行：`file '<绝对路径>'`（单引号转义；遵循 ffmpeg concat demuxer 的 list.txt 格式）。
pub fn concat_list_line(path: &Path) -> String {
    let escaped = path
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "'\\''");
    format!("file '{escaped}'")
}

impl<'a> HlsMaterializer<'a> {
    /// 拉取并解析播放列表文本。
    async fn fetch_text(&self, url: &str, work: &Path, name: &str) -> Result<String, HlsFailure> {
        let dest = work.join(name);
        let headers = crate::headers::resolve_request_headers(
            self.sources,
            &crate::headers::HeaderTarget {
                url,
                page_url: self.page_url,
            },
        )
        .sent;
        self.downloader
            .download(url, &dest, &headers)
            .await
            .map_err(HlsFailure::Download)?;
        std::fs::read_to_string(&dest).map_err(|error| HlsFailure::Io(error.to_string()))
    }

    /// 把 HLS 落成单个 mp4：`playlist_url` 可以是 master 或 media 清单。
    ///
    /// 返回**本次真实网络字节数**（清单 + 分片），产物大小由调用方读元数据。
    pub async fn materialize(&self, playlist_url: &str, dest: &Path) -> Result<u64, HlsFailure> {
        if playlist_url.trim().is_empty() {
            return Err(HlsFailure::Parse(HlsError::NotAPlaylist));
        }
        let concurrency = normalize_hls_concurrency(Some(self.concurrency));
        let work =
            WorkDir::create_near(dest, "hls").map_err(|error| HlsFailure::Io(error.to_string()))?;
        self.materialize_in(work.path(), playlist_url, dest, concurrency)
            .await
    }

    async fn materialize_in(
        &self,
        work: &Path,
        playlist_url: &str,
        dest: &Path,
        concurrency: usize,
    ) -> Result<u64, HlsFailure> {
        let mut network_bytes = 0u64;
        let mut text = self.fetch_text(playlist_url, work, "playlist.m3u8").await?;
        network_bytes += text.len() as u64;

        let mut media_url = playlist_url.to_string();
        let parsed = parse_playlist(&text, &media_url)?;
        let segments = match parsed {
            Playlist::Master { variants } => {
                // master → 选最高清晰度 → 再取 media 清单。
                let picked = highest_bandwidth_variant(&variants)?.url.clone();
                media_url = picked.clone();
                text = self.fetch_text(&picked, work, "variant.m3u8").await?;
                network_bytes += text.len() as u64;
                match parse_playlist(&text, &media_url)? {
                    Playlist::Media { segments } => segments,
                    Playlist::Master { .. } => {
                        return Err(HlsFailure::Parse(HlsError::NoSegments));
                    }
                }
            }
            Playlist::Media { segments } => segments,
        };

        let seg_dir = work.join("segs");
        std::fs::create_dir_all(&seg_dir).map_err(|error| HlsFailure::Io(error.to_string()))?;

        // 分片并发下载（HLS_CONCURRENCY，默认 16，钳 1..=32）。
        // **分片失败必须冒泡**：HLS 少一个分片就是不完整成片，绝不静默跳过。
        let results: Vec<Result<u64, HlsFailure>> =
            futures::stream::iter(segments.clone().into_iter().enumerate())
                .map(|(index, url)| {
                    let seg_dir = &seg_dir;
                    async move {
                        let headers = crate::headers::resolve_request_headers(
                            self.sources,
                            &crate::headers::HeaderTarget {
                                url: &url,
                                page_url: self.page_url,
                            },
                        )
                        .sent;
                        let dest = seg_dir.join(segment_name(index));
                        let bytes = self
                            .downloader
                            .download(&url, &dest, &headers)
                            .await
                            .map_err(HlsFailure::Download)?;
                        Ok::<u64, HlsFailure>(bytes)
                    }
                })
                .buffer_unordered(concurrency)
                .collect()
                .await;
        for item in results {
            network_bytes += item?;
        }

        // concat 清单：每个分片一行 `file '<path>'`。
        let list_path = work.join("list.txt");
        let list = segments
            .iter()
            .enumerate()
            .map(|(index, _)| concat_list_line(&seg_dir.join(segment_name(index))))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&list_path, list).map_err(|error| HlsFailure::Io(error.to_string()))?;

        let tmp_out = work.join("out.mp4");
        self.ffmpeg.concat(&list_path, &tmp_out).await?;

        // 封装成功但产物缺失与空文件同归「空输出」：都不能交付。
        let size = std::fs::metadata(&tmp_out)
            .map(|info| info.len())
            .unwrap_or_default();
        if size == 0 {
            return Err(HlsFailure::EmptyOutput);
        }
        commit(&tmp_out, dest).map_err(|error| HlsFailure::Io(error.to_string()))?;
        Ok(network_bytes)
    }
}

/// 供 `PathBuf` 使用的小工具：`work` 目录下的分片路径。
pub fn segment_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(segment_name(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::CommandRunner;
    use crate::transfer::progress::TransferReport;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeDownloader {
        /// url → 内容；记录每次请求的目标 URL 与请求头。
        files: Mutex<HashMap<String, Vec<u8>>>,
        calls: Mutex<Vec<(String, HashMap<String, String>)>>,
        /// url → 本次落盘目标；用于断言临时目录与 dest 同父目录（同卷）。
        paths: Mutex<Vec<(String, PathBuf)>>,
    }

    impl FakeDownloader {
        fn with(map: Vec<(&str, &str)>) -> Arc<Self> {
            let this = Arc::new(Self::default());
            for (url, content) in map {
                this.files
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), content.as_bytes().to_vec());
            }
            this
        }
    }

    #[async_trait::async_trait]
    impl FileDownloader for FakeDownloader {
        async fn download(
            &self,
            url: &str,
            dest: &Path,
            headers: &HashMap<String, String>,
        ) -> Result<u64, DownloadError> {
            self.calls
                .lock()
                .unwrap()
                .push((url.to_string(), headers.clone()));
            self.paths
                .lock()
                .unwrap()
                .push((url.to_string(), dest.to_path_buf()));
            let body = self
                .files
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| DownloadError::Http {
                    status: 404,
                    detail: "not found".into(),
                })?;
            std::fs::write(dest, &body).map_err(|error| DownloadError::Io(error.to_string()))?;
            Ok(body.len() as u64)
        }

        async fn download_tracked(
            &self,
            _track: &crate::transfer::progress::TransferTrack,
            url: &str,
            dest: &Path,
            headers: &HashMap<String, String>,
            _page_url: Option<&str>,
            _progress: Option<&dyn crate::traits::ProgressSink>,
        ) -> Result<TransferReport, DownloadError> {
            let bytes = self.download(url, dest, headers).await?;
            Ok(TransferReport {
                network_bytes: bytes,
                validated_bytes: bytes,
                attempts: 1,
                resumed_bytes: 0,
                total_bytes: Some(bytes),
            })
        }
    }

    struct FakeFfmpeg {
        ran: Mutex<Vec<Vec<String>>>,
        status: i32,
        /// Some(bytes) = 写产物；None 表示不写（空输出）。
        output: Option<Vec<u8>>,
        /// 失败前先写半成品：模拟 `-y` 直接写目标时把旧成片截断。
        partial_on_failure: bool,
    }
    impl FakeFfmpeg {
        fn with(status: i32, output: Option<Vec<u8>>, partial_on_failure: bool) -> Arc<Self> {
            Arc::new(Self {
                ran: Mutex::new(Vec::new()),
                status,
                output,
                partial_on_failure,
            })
        }
        fn ok() -> Arc<Self> {
            Self::with(0, Some(vec![0u8; 2048]), false)
        }
        fn failing() -> Arc<Self> {
            Self::with(1, None, true)
        }
        fn empty() -> Arc<Self> {
            Self::with(0, None, false)
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for FakeFfmpeg {
        async fn run(
            &self,
            _program: &Path,
            args: &[String],
            _timeout: std::time::Duration,
        ) -> Result<crate::ffmpeg::ProcessOutput, FfmpegError> {
            self.ran.lock().unwrap().push(args.to_vec());
            if let Some(out) = args.last() {
                match &self.output {
                    Some(bytes) => {
                        let _ = std::fs::write(out, bytes);
                    }
                    None if self.partial_on_failure => {
                        let _ = std::fs::write(out, "半成品".as_bytes());
                    }
                    None => {}
                }
            }
            Ok(crate::ffmpeg::ProcessOutput {
                status: self.status,
                stderr: if self.status == 0 {
                    String::new()
                } else {
                    "ffmpeg 失败".into()
                },
                ..Default::default()
            })
        }
    }

    fn sources() -> crate::headers::HeaderSources {
        crate::headers::HeaderSources::default()
    }

    #[tokio::test]
    async fn hls落地用concat封装且绝不交付播放列表() {
        let downloader = FakeDownloader::with(vec![
            (
                "https://cdn.test/hls/master.m3u8",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=900\nhigh.m3u8\n",
            ),
            (
                "https://cdn.test/hls/high.m3u8",
                "#EXTM3U\n#EXTINF:9.9,\nseg0.ts\n#EXTINF:9.9,\nseg1.ts\n#EXT-X-ENDLIST\n",
            ),
            ("https://cdn.test/hls/seg0.ts", "AAAAAAAA"),
            ("https://cdn.test/hls/seg1.ts", "BBBBBBBB"),
        ]);
        let runner = FakeFfmpeg::ok();
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");

        let materializer = HlsMaterializer {
            downloader: downloader.as_ref(),
            ffmpeg: &ffmpeg,
            sources: &sources(),
            page_url: None,
            concurrency: 4,
            budget: None,
        };
        materializer
            .materialize("https://cdn.test/hls/master.m3u8", &dest)
            .await
            .unwrap();

        assert!(dest.exists(), "产物是封装后的 mp4");
        assert!(!std::fs::read(&dest).unwrap().is_empty());
        // 只下了最高清晰度的分片，低清晰度分片不该被请求。
        let calls: Vec<String> = downloader
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(url, _)| url.clone())
            .collect();
        assert!(calls.contains(&"https://cdn.test/hls/high.m3u8".to_string()));
        assert!(calls.contains(&"https://cdn.test/hls/seg0.ts".to_string()));
        assert!(!calls.contains(&"https://cdn.test/hls/low.m3u8".to_string()));
        let ran = runner.ran.lock().unwrap();
        assert_eq!(ran.len(), 1, "只跑一次 concat");
        let list = ran[0]
            .iter()
            .find(|arg| arg.ends_with("list.txt"))
            .expect("concat 要吃 list.txt");
        assert!(list.contains("list.txt"));
    }

    #[tokio::test]
    async fn 加密hls明确失败() {
        let downloader = FakeDownloader::with(vec![(
            "https://cdn.test/hls/a.m3u8",
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:1,\ns.ts\n",
        )]);
        let runner = FakeFfmpeg::ok();
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        let dir = tempfile::tempdir().unwrap();
        let materializer = HlsMaterializer {
            downloader: downloader.as_ref(),
            ffmpeg: &ffmpeg,
            sources: &sources(),
            page_url: None,
            concurrency: 2,
            budget: None,
        };
        let error = materializer
            .materialize("https://cdn.test/hls/a.m3u8", &dir.path().join("o.mp4"))
            .await
            .unwrap_err();
        assert!(
            matches!(error, HlsFailure::Parse(HlsError::Encrypted)),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "播放列表已加密，无法直接下载");
    }

    #[tokio::test]
    async fn 分片失败不静默交付残缺成片() {
        let downloader = FakeDownloader::with(vec![
            (
                "https://cdn.test/hls/a.m3u8",
                "#EXTM3U\n#EXTINF:1,\nseg0.ts\n#EXTINF:1,\nseg1.ts\n#EXT-X-ENDLIST\n",
            ),
            ("https://cdn.test/hls/seg0.ts", "AAAA"),
            // seg1 缺失 → 404
        ]);
        let runner = FakeFfmpeg::ok();
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        let dir = tempfile::tempdir().unwrap();
        let materializer = HlsMaterializer {
            downloader: downloader.as_ref(),
            ffmpeg: &ffmpeg,
            sources: &sources(),
            page_url: None,
            concurrency: 2,
            budget: None,
        };
        let error = materializer
            .materialize("https://cdn.test/hls/a.m3u8", &dir.path().join("o.mp4"))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("下载失败") || error.to_string().contains("封装产物为空"),
            "分片缺失必须失败，不能拼出残缺成片: {error}"
        );
    }

    #[test]
    fn hls并发钳制与ts一致() {
        assert_eq!(normalize_hls_concurrency(None), 16);
        assert_eq!(normalize_hls_concurrency(Some(0)), 16);
        assert_eq!(normalize_hls_concurrency(Some(99)), 32);
        assert_eq!(normalize_hls_concurrency(Some(4)), 4);
    }

    #[test]
    fn concat清单行转义单引号() {
        let line = concat_list_line(Path::new("/tmp/a'b.ts"));
        assert_eq!(line, "file '/tmp/a'\\''b.ts'");
    }

    /// 标准 media 清单 + 一个分片；产物由 FakeFfmpeg 决定。
    fn standard_downloader() -> Arc<FakeDownloader> {
        FakeDownloader::with(vec![
            (
                "https://cdn.test/hls/a.m3u8",
                "#EXTM3U\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n",
            ),
            ("https://cdn.test/hls/seg0.ts", "AAAA"),
        ])
    }

    fn named_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// 结构断言：清单/分片/封装输入输出都在 dest 父目录下的同一个工作目录里。
    /// 不声称真实 Windows 跨卷已验收；跨卷根因就是「工作目录不在 dest 同卷」。
    #[tokio::test]
    async fn hls临时目录必须与目标同父目录且产物先落临时再交付() {
        let downloader = standard_downloader();
        let runner = FakeFfmpeg::ok();
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");
        let sources = sources();
        let materializer = HlsMaterializer {
            downloader: downloader.as_ref(),
            ffmpeg: &ffmpeg,
            sources: &sources,
            page_url: None,
            concurrency: 2,
            budget: None,
        };
        materializer
            .materialize("https://cdn.test/hls/a.m3u8", &dest)
            .await
            .unwrap();

        let ran = runner.ran.lock().unwrap();
        assert_eq!(ran.len(), 1);
        let list = PathBuf::from(ran[0].iter().find(|arg| arg.ends_with("list.txt")).unwrap());
        let out = PathBuf::from(ran[0].last().unwrap());
        let work = out.parent().unwrap();
        assert_eq!(work.parent(), dest.parent(), "工作目录必须在 dest 父目录下");
        assert_eq!(list.parent(), Some(work), "concat 清单与产物同目录");
        assert_ne!(out, dest, "先写临时产物，再由 rename 交付");

        let paths = downloader.paths.lock().unwrap();
        let playlist_dest = &paths
            .iter()
            .find(|(url, _)| url.ends_with("a.m3u8"))
            .unwrap()
            .1;
        assert_eq!(playlist_dest.parent(), Some(work), "清单也在同一工作目录");
        let segment_dest = &paths
            .iter()
            .find(|(url, _)| url.ends_with("seg0.ts"))
            .unwrap()
            .1;
        assert!(segment_dest.starts_with(work), "分片不能落到系统临时目录");
        assert_eq!(
            named_entries(dir.path()),
            ["out.mp4"],
            "成功后必须清理本次工作目录"
        );
        assert!(!std::fs::read(&dest).unwrap().is_empty());
    }

    #[tokio::test]
    async fn hls封装失败或空输出保留已有成片() {
        for runner in [FakeFfmpeg::failing(), FakeFfmpeg::empty()] {
            let downloader = standard_downloader();
            let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner);
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("out.mp4");
            std::fs::write(&dest, b"GOOD").unwrap();
            let sources = sources();
            let materializer = HlsMaterializer {
                downloader: downloader.as_ref(),
                ffmpeg: &ffmpeg,
                sources: &sources,
                page_url: None,
                concurrency: 2,
                budget: None,
            };
            let error = materializer
                .materialize("https://cdn.test/hls/a.m3u8", &dest)
                .await
                .unwrap_err();
            assert!(
                matches!(error, HlsFailure::Ffmpeg(_) | HlsFailure::EmptyOutput),
                "封装失败/空输出必须明确失败: {error}"
            );
            assert_eq!(
                std::fs::read(&dest).unwrap(),
                b"GOOD",
                "失败不能删除已有成片"
            );
            assert_eq!(
                named_entries(dir.path()),
                ["out.mp4"],
                "失败的本次工作目录也必须清理"
            );
        }
    }

    #[tokio::test]
    async fn hls交付目标被占用时保留原目标不预删() {
        let downloader = standard_downloader();
        let runner = FakeFfmpeg::ok();
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");
        // 目标被占成非空目录：rename 失败，但绝不能先删掉目标。
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("旧内容"), b"GOOD").unwrap();
        let sources = sources();
        let materializer = HlsMaterializer {
            downloader: downloader.as_ref(),
            ffmpeg: &ffmpeg,
            sources: &sources,
            page_url: None,
            concurrency: 2,
            budget: None,
        };
        assert!(materializer
            .materialize("https://cdn.test/hls/a.m3u8", &dest)
            .await
            .is_err());
        assert_eq!(std::fs::read(dest.join("旧内容")).unwrap(), b"GOOD");
    }

    #[tokio::test]
    async fn hls并发任务各自使用独立同卷工作目录() {
        let downloader = standard_downloader();
        let runner = FakeFfmpeg::ok();
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner.clone());
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.mp4");
        let second = dir.path().join("b.mp4");
        let sources = sources();
        let one = HlsMaterializer {
            downloader: downloader.as_ref(),
            ffmpeg: &ffmpeg,
            sources: &sources,
            page_url: None,
            concurrency: 2,
            budget: None,
        };
        let two = HlsMaterializer {
            downloader: downloader.as_ref(),
            ffmpeg: &ffmpeg,
            sources: &sources,
            page_url: None,
            concurrency: 2,
            budget: None,
        };
        let (first_result, second_result) = futures::join!(
            one.materialize("https://cdn.test/hls/a.m3u8", &first),
            two.materialize("https://cdn.test/hls/a.m3u8", &second)
        );
        first_result.unwrap();
        second_result.unwrap();
        let ran = runner.ran.lock().unwrap();
        assert_eq!(ran.len(), 2);
        let works: Vec<PathBuf> = ran
            .iter()
            .map(|args| {
                PathBuf::from(args.last().unwrap())
                    .parent()
                    .unwrap()
                    .to_path_buf()
            })
            .collect();
        assert_ne!(works[0], works[1], "并发落地不能共用临时目录");
        assert!(works.iter().all(|work| work.parent() == Some(dir.path())));
        assert_eq!(named_entries(dir.path()), ["a.mp4", "b.mp4"]);
    }

    struct BlockingSegments {
        playlist: String,
        dests: Mutex<Vec<PathBuf>>,
    }
    #[async_trait::async_trait]
    impl FileDownloader for BlockingSegments {
        async fn download(
            &self,
            url: &str,
            dest: &Path,
            _headers: &HashMap<String, String>,
        ) -> Result<u64, DownloadError> {
            self.dests.lock().unwrap().push(dest.to_path_buf());
            if url.ends_with(".m3u8") {
                std::fs::write(dest, &self.playlist)
                    .map_err(|error| DownloadError::Io(error.to_string()))?;
                Ok(self.playlist.len() as u64)
            } else {
                futures::future::pending::<()>().await;
                unreachable!("分片下载会被取消")
            }
        }
    }

    #[tokio::test]
    async fn hls取消future时自动清理本次同卷工作目录() {
        let downloader = BlockingSegments {
            playlist: "#EXTM3U\n#EXTINF:1,\nseg0.ts\n#EXT-X-ENDLIST\n".into(),
            dests: Mutex::new(Vec::new()),
        };
        let runner = FakeFfmpeg::ok();
        let ffmpeg = Ffmpeg::with_runner(PathBuf::from("ffmpeg"), runner);
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.mp4");
        let sources = sources();
        let materializer = HlsMaterializer {
            downloader: &downloader,
            ffmpeg: &ffmpeg,
            sources: &sources,
            page_url: None,
            concurrency: 2,
            budget: None,
        };
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            materializer.materialize("https://cdn.test/hls/a.m3u8", &dest),
        )
        .await;
        assert!(cancelled.is_err(), "分片一直不返回，应被超时取消");
        let work = downloader.dests.lock().unwrap()[0]
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(!work.exists(), "取消后临时目录必须被清理");
        assert!(named_entries(dir.path()).is_empty());
    }
}
