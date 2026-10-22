//! 下载任务：**SDK 与可执行文件共用的字段真源**。
//!
//! 一行 JSON 就是一个任务（JSON Lines 输入），Rust 调用方直接构造同名结构体；
//! 两种形态的字段、默认值、语义完全一致，没有第二份定义。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;

/// 一个下载任务。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    /// 调用方自己的标识，原样回传（可重复，不参与去重）。
    #[serde(default)]
    pub id: String,

    /// 主直链（HTTP/HTTPS）。HLS 播放列表（`#EXTM3U`）会被自动识别并分片封装。
    pub url: String,

    /// 备用直链：主地址失败时按顺序回退（签名过期、边缘节点拒绝时有用）。
    #[serde(default)]
    pub urls: Vec<String>,

    /// 音频直链：给出即按 DASH 处理——先下两条流，再 ffmpeg 合并。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_url: Option<String>,

    /// 产物落盘路径（含文件名）。父目录不存在会自动创建。
    pub out: PathBuf,

    /// 显式请求头。**空串表示明确不发送该头**；同层内大小写无关。
    ///
    /// 未给 `Referer` 时按目标域名自动推断（内置平台矩阵），
    /// 跨域重定向会按每一跳重新推断。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,

    /// 原始内容页 URL：目标域名识别不出平台时用它的 origin 兜底 Referer。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,

    /// 是否用 ffmpeg 校验产物；省略时用全局配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<bool>,

    /// 是否计算 MD5；省略时用全局配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<bool>,
}

impl Job {
    /// 任务级校验。返回的错误码固定为 `INVALID_JOB`。
    pub fn validate(&self) -> Result<(), (ErrorCode, String)> {
        if self.url.trim().is_empty() {
            return Err((ErrorCode::InvalidJob, "url 不能为空".into()));
        }
        if !is_http_url(&self.url) {
            return Err((
                ErrorCode::InvalidJob,
                format!("url 不是 HTTP(S) 地址: {}", self.url),
            ));
        }
        if self.out.as_os_str().is_empty() {
            return Err((ErrorCode::InvalidJob, "out 不能为空".into()));
        }
        if let Some(audio) = self.audio_url.as_deref() {
            if audio.trim().is_empty() {
                return Err((ErrorCode::InvalidJob, "audio_url 不能为空串".into()));
            }
            if !is_http_url(audio) {
                return Err((
                    ErrorCode::InvalidJob,
                    format!("audio_url 不是 HTTP(S) 地址: {audio}"),
                ));
            }
        }
        for url in &self.urls {
            if url.trim().is_empty() || !is_http_url(url) {
                return Err((ErrorCode::InvalidJob, format!("urls 中存在无效地址: {url}")));
            }
        }
        Ok(())
    }

    /// 候选视频地址：主地址在前，备用去重后追加。
    pub fn candidates(&self) -> Vec<&str> {
        let mut candidates = vec![self.url.as_str()];
        for url in &self.urls {
            if !candidates.contains(&url.as_str()) {
                candidates.push(url.as_str());
            }
        }
        candidates
    }

    /// 是否 DASH 双流任务。
    pub fn requires_merge(&self) -> bool {
        self.audio_url.is_some()
    }
}

fn is_http_url(value: &str) -> bool {
    matches!(
        url::Url::parse(value.trim()).map(|url| url.scheme().to_string()),
        Ok(scheme) if scheme == "http" || scheme == "https"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 字段与json完全一致且拒绝未知字段() {
        let job: Job = serde_json::from_str(
            r#"{"id":"a","url":"https://cdn.example.com/v.mp4","out":"/tmp/v.mp4",
                "headers":{"Referer":""},"page_url":"https://example.com/v/1"}"#,
        )
        .unwrap();
        assert_eq!(job.id, "a");
        assert_eq!(job.headers["Referer"], "");
        assert!(job.validate().is_ok());

        let error = serde_json::from_str::<Job>(
            r#"{"url":"https://cdn.example.com/v.mp4","out":"/tmp/v.mp4","typo":1}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn 候选地址主地址在前且自动去重() {
        let job = Job {
            url: "https://a/1".into(),
            urls: vec!["https://a/1".into(), "https://b/2".into()],
            out: "/tmp/x".into(),
            ..Default::default()
        };
        assert_eq!(job.candidates(), ["https://a/1", "https://b/2"]);
    }

    #[test]
    fn 缺少地址或非http地址判为非法任务() {
        for job in [
            Job {
                url: "".into(),
                out: "/tmp/x".into(),
                ..Default::default()
            },
            Job {
                url: "ftp://example.com/x".into(),
                out: "/tmp/x".into(),
                ..Default::default()
            },
            Job {
                url: "https://example.com/x".into(),
                out: "".into(),
                ..Default::default()
            },
        ] {
            let (code, _) = job.validate().unwrap_err();
            assert_eq!(code, ErrorCode::InvalidJob);
        }
    }

    #[test]
    fn 音频地址存在即dash且空串非法() {
        let mut job = Job {
            url: "https://example.com/v".into(),
            audio_url: Some("https://example.com/a".into()),
            out: "/tmp/x".into(),
            ..Default::default()
        };
        assert!(job.requires_merge());
        job.audio_url = Some(" ".into());
        assert_eq!(job.validate().unwrap_err().0, ErrorCode::InvalidJob);
    }
}
