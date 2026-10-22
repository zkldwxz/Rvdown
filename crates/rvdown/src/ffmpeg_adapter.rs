//! 外置 ffmpeg 接回可注入 trait，临时产物校验通过后才替换目标。

use crate::{
    error::{MuxError, ProbeError},
    ffmpeg::{Ffmpeg, FfmpegError},
    traits::{AudioMuxer, FrameGrabber, VideoProbe},
    types::VideoProbeResult,
};
use async_trait::async_trait;
use std::{io, path::Path};

async fn output_file(dest: &Path, suffix: &str) -> Result<tempfile::NamedTempFile, io::Error> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    tokio::fs::create_dir_all(parent).await?;
    tempfile::Builder::new()
        .prefix(".ffmpeg-")
        .suffix(suffix)
        .tempfile_in(parent)
}
async fn publish(output: tempfile::NamedTempFile, dest: &Path) -> Result<(), io::Error> {
    if tokio::fs::metadata(output.path()).await?.len() == 0 {
        return Err(io::Error::other("临时产物为空，保留原文件"));
    }
    tokio::fs::File::from_std(output.as_file().try_clone()?)
        .sync_all()
        .await?;
    output.persist(dest).map_err(|error| error.error)?;
    Ok(())
}
async fn aliases(dest: &Path, source: &Path) -> bool {
    dest == source
        || match (
            tokio::fs::canonicalize(dest).await,
            tokio::fs::canonicalize(source).await,
        ) {
            (Ok(dest), Ok(source)) => dest == source,
            _ => false,
        }
}
fn mux_error(error: FfmpegError) -> MuxError {
    match error {
        FfmpegError::MissingBinary => MuxError::MissingBinary,
        other => MuxError::Failed(other.to_string()),
    }
}
fn probe_error(error: FfmpegError) -> ProbeError {
    match error {
        FfmpegError::MissingBinary => ProbeError::MissingBinary,
        other => ProbeError::Failed(other.to_string()),
    }
}
#[async_trait]
impl AudioMuxer for Ffmpeg {
    async fn mux(&self, video: &Path, audio: &Path, dest: &Path) -> Result<(), MuxError> {
        if aliases(dest, video).await || aliases(dest, audio).await {
            return Err(MuxError::Failed("合并目标不能覆盖视频或音频输入".into()));
        }
        let output = output_file(dest, ".mp4")
            .await
            .map_err(|e| MuxError::Failed(e.to_string()))?;
        Ffmpeg::mux(self, video, audio, output.path())
            .await
            .map_err(mux_error)?;
        publish(output, dest)
            .await
            .map_err(|e| MuxError::Failed(e.to_string()))
    }
}
#[async_trait]
impl VideoProbe for Ffmpeg {
    async fn probe(&self, path: &Path) -> Result<VideoProbeResult, ProbeError> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|e| ProbeError::Failed(e.to_string()))?;
        if !metadata.is_file() {
            return Err(ProbeError::Failed("产物不是普通文件".into()));
        }
        let reading = Ffmpeg::probe(self, path).await.map_err(probe_error)?;
        Ok(VideoProbeResult {
            size_bytes: metadata.len(),
            has_video: reading.has_video,
            has_audio: reading.has_audio,
            duration_seconds: reading.duration_seconds,
            detail: reading.detail,
        })
    }
}
#[async_trait]
impl FrameGrabber for Ffmpeg {
    async fn capture_frame(
        &self,
        video: &Path,
        dest: &Path,
        at_seconds: f64,
    ) -> Result<(), ProbeError> {
        if aliases(dest, video).await {
            return Err(ProbeError::Failed("封面目标不能覆盖视频输入".into()));
        }
        let output = output_file(dest, ".jpg")
            .await
            .map_err(|e| ProbeError::Failed(e.to_string()))?;
        Ffmpeg::grab_frame(self, video, output.path(), at_seconds)
            .await
            .map_err(probe_error)?;
        publish(output, dest)
            .await
            .map_err(|e| ProbeError::Failed(e.to_string()))
    }
}

/// 合并入口（任务执行器用）：先写同目录临时文件，非空且落盘后才替换 `dest`。
pub(crate) async fn mux_files(
    ffmpeg: &Ffmpeg,
    video: &Path,
    audio: &Path,
    dest: &Path,
) -> Result<(), MuxError> {
    <Ffmpeg as AudioMuxer>::mux(ffmpeg, video, audio, dest).await
}

#[cfg(test)]
mod tests {
    use crate::{ffmpeg::*, traits::*};
    use async_trait::async_trait;
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::Duration,
    };

    struct Runner {
        status: i32,
        bytes: Vec<u8>,
        calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
    }
    #[async_trait]
    impl CommandRunner for Runner {
        async fn run(
            &self,
            program: &Path,
            args: &[String],
            _: Duration,
        ) -> Result<ProcessOutput, FfmpegError> {
            self.calls
                .lock()
                .unwrap()
                .push((program.into(), args.to_vec()));
            if args.iter().any(|arg| arg == "-y") {
                std::fs::write(args.last().unwrap(), &self.bytes).unwrap();
            }
            Ok(ProcessOutput {
                status: self.status,
                stdout: vec![],
                stderr: "Duration: 00:00:10.25\nStream #0:0: Video: h264\nStream #0:1: Audio: aac"
                    .into(),
            })
        }
    }
    fn runner(status: i32, bytes: &[u8]) -> Arc<Runner> {
        Arc::new(Runner {
            status,
            bytes: bytes.into(),
            calls: Mutex::new(vec![]),
        })
    }
    #[test]
    fn 随包组件从程序目录定位且用户显式路径优先() {
        let root = tempfile::tempdir().unwrap();
        let resources = root.path().join("resources");
        std::fs::create_dir(&resources).unwrap();
        let bundled = resources.join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        std::fs::write(&bundled, "组件").unwrap();
        assert_eq!(Ffmpeg::resolve_program_in(root.path(), None), bundled);
        let explicit = root.path().join("另一个 ffmpeg.exe");
        assert_eq!(
            Ffmpeg::resolve_program_in(root.path(), Some(&format!("\"{}\"", explicit.display()))),
            explicit
        );
    }
    #[tokio::test]
    async fn 合并失败或空产物保留原文件成功后才原子替换且不覆盖视频音频输入() {
        for (status, bytes, success) in [
            (1, "残件".as_bytes(), false),
            (0, "".as_bytes(), false),
            (0, "成片".as_bytes(), true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let video = root.path().join("v.tmp");
            let audio = root.path().join("a.tmp");
            let dest = root.path().join("成片.mp4");
            std::fs::write(&video, "视频输入").unwrap();
            std::fs::write(&audio, "音频输入").unwrap();
            std::fs::write(&dest, "原有成片").unwrap();
            let runner = runner(status, bytes);
            let ffmpeg = Ffmpeg::with_runner("ffmpeg".into(), runner.clone());
            let muxer: &dyn AudioMuxer = &ffmpeg;
            assert_eq!(muxer.mux(&video, &audio, &dest).await.is_ok(), success);
            assert_eq!(
                std::fs::read_to_string(&dest).unwrap(),
                if success { "成片" } else { "原有成片" }
            );
            assert_eq!(std::fs::read_to_string(&video).unwrap(), "视频输入");
            assert_eq!(std::fs::read_to_string(&audio).unwrap(), "音频输入");
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
            let calls = runner.calls.lock().unwrap();
            let output = Path::new(calls[0].1.last().unwrap());
            assert_eq!(output.parent(), dest.parent(), "临时输出和成片必须同卷");
            assert_eq!(output.extension().unwrap(), "mp4");
        }
    }
    #[tokio::test]
    async fn 产物检验通过真实文件大小和完整流信息() {
        let root = tempfile::tempdir().unwrap();
        let video = root.path().join("视频.mp4");
        std::fs::write(&video, vec![1u8; 20_000]).unwrap();
        let ffmpeg = Ffmpeg::with_runner("ffmpeg".into(), runner(1, b""));
        let probe: &dyn VideoProbe = &ffmpeg;
        let actual = probe.probe(&video).await.unwrap();
        assert_eq!(actual.size_bytes, 20_000);
        assert!(actual.has_video && actual.has_audio);
        assert_eq!(actual.duration_seconds, Some(10.25));
    }
    #[tokio::test]
    async fn 截帧失败或空输出不覆盖原封面且使用指定秒数() {
        for (status, bytes) in [
            (1, "错误".as_bytes()),
            (0, "".as_bytes()),
            (0, "图片".as_bytes()),
        ] {
            let root = tempfile::tempdir().unwrap();
            let video = root.path().join("视频.mp4");
            let dest = root.path().join("封面.jpg");
            std::fs::write(&video, "视频").unwrap();
            std::fs::write(&dest, "旧封面").unwrap();
            let runner = runner(status, bytes);
            let ffmpeg = Ffmpeg::with_runner("ffmpeg".into(), runner.clone());
            let grabber: &dyn FrameGrabber = &ffmpeg;
            let success = status == 0 && !bytes.is_empty();
            assert_eq!(
                grabber.capture_frame(&video, &dest, 3.0).await.is_ok(),
                success
            );
            assert_eq!(
                std::fs::read_to_string(&dest).unwrap(),
                if success { "图片" } else { "旧封面" }
            );
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
            assert!(runner.calls.lock().unwrap()[0]
                .1
                .iter()
                .any(|arg| arg == "3.000"));
        }
    }
}
