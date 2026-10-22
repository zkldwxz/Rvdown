# Rvdown

[实测对比报告 / measured comparison](docs/benchmarks/2026-10-22/report.zh-CN.md) · [Reproduce](benchmarks/README.md)

面向脚本化流水线的通用多线程视频下载器——在爬取、审核、归档这类 CDN 会主动反爬的场景里
替代 `aria2`。

[English](README.md) · [调用文档](docs/calling.zh-CN.md) · [错误码](docs/error-codes.md)

## 它解决什么

- **分段 Range 下载**：N 路并行按字节区间写 `.part`，收齐后原子改名落目标文件。跨进程重启
  按侧车台账续传（`.aria2` 命名），**资源中途变了绝不拼片**（校验 ETag / Last-Modified /
  长度 / 重定向最终地址）。
- **浏览器 TLS 指纹**：抖音系与部分国内 CDN 对非浏览器 TLS 栈（aria2、Node `fetch`、
  Python `requests`）直接返回 403。Rvdown 内置 Chrome 131 的 JA3 档位，这类直链可以直接拉。
- **自动识别 HLS**：播放列表 URL 会被识别、分片并发下载、再用 ffmpeg 封装成真正的媒体文件；
  **绝不把播放列表当成品交付**。
- **自动 DASH**：给了 `audio_url` 就是双流任务——两条流都下完再 ffmpeg 合并（流拷贝）。
  缺音频流是硬失败，绝不交付没有声音的成片。
- **内置 Referer / UA 矩阵**：bilibili / 快手 / 头条 / 抖音 / 小红书按域名自动补 Referer，
  **每一跳重定向都重新计算**；任务级 `headers` 可覆盖任意头，空值表示「明确不发送」。
- **产物校验**：大小下限 + ffmpeg 流信息探测。错误页、截断文件会被判失败并清理，不会当成功。
- **两种形态一套契约**：Rust SDK 与单文件可执行程序，字段名和错误码完全一致。

## 安装

```bash
# 预编译（Windows x64、macOS arm64/x64、Linux x64）
# https://github.com/zkldwxz/Rvdown/releases

# Rust SDK：钉 tag，不要跟分支
cargo add rvdown --git https://github.com/zkldwxz/Rvdown --tag v0.1.0
```

ffmpeg 外置且可选：程序同目录 `resources/` 下、或 `PATH` 里、或显式传入（`--ffmpeg` /
`Config::ffmpeg`）。没有 ffmpeg 时不能做 DASH 合并，产物校验退化为大小检查。

## 命令行（任何语言）

```bash
rvdown get "https://cdn.example.com/video.mp4" --out ./video.mp4

# 批量：一行一个任务，结果逐条流式返回
rvdown run --tasks jobs.jsonl --out results.jsonl --progress
```

```jsonc
// jobs.jsonl
{"id":"a","url":"https://cdn.example.com/v.mp4","out":"/data/a.mp4"}
{"id":"b","url":"https://cdn.example.com/main.m3u8","out":"/data/b.mp4","headers":{"Cookie":"s=1"}}
{"id":"c","url":"https://cdn.example.com/video.m4s","audio_url":"https://cdn.example.com/audio.m4s","out":"/data/c.mp4"}
{"id":"d","url":"https://cdn.example.com/v.mp4","urls":["https://mirror.example.com/v.mp4"],"out":"/data/d.mp4"}
```

退出码：`0` 全部成功 · `1` 有任务失败（结果完整）· `2` 参数/任务文件错误（未执行）·
`3` 环境致命 · `130` 被中断（已完成文件保留，`.part` 可续传）。

## Rust SDK

```rust
use rvdown::{Client, Job};

#[tokio::main]
async fn main() -> Result<(), rvdown::JobError> {
    let client = Client::builder()
        .connections(16)        // 单文件分段并发
        .job_concurrency(4)     // 同时下载的文件数
        .build()?;

    let job = Job {
        id: "a".into(),
        url: "https://cdn.example.com/video.mp4".into(),
        out: "/data/a.mp4".into(),
        ..Default::default()
    };
    let outcome = client.run(&job).await;
    assert!(outcome.ok, "{}: {:?}", outcome.code.description(), outcome.detail);
    println!("{:?} bytes={:?} md5={:?}", outcome.code, outcome.bytes, outcome.md5);
    Ok(())
}
```

需要自己编排时可以直接用下层：`MediaDownloader`（带 HLS 识别的文件传输）、
`RangeDownloader` + 可注入 `SegmentDownloader`、以及 ffmpeg 适配器
（`AudioMuxer` / `VideoProbe` / `FrameGrabber`）。

## 行为约定

- 任务自身的 `headers` 优先于全局 `--header`；空值表示彻底不发这个头（应对拒绝 Referer 的 CDN）。
- 进度字节拆成三类：已验证 / 断点恢复 / 本次网络。重试不让百分比假涨，恢复字节不算网速。
- 永久性文件系统错误（磁盘满、权限、路径）立即失败；瞬时故障（403/408/425/429/5xx、
  网络重置）先按 `Retry-After`、再按指数退避重试。
- 同一目标已被别的任务写入时返回 `TARGET_BUSY`，绝不并发抢写。

## 许可

CC BY-NC-SA 4.0 —— 仅限非商业使用；二次开发必须使用相同许可分发。见 [LICENSE](LICENSE)。
这是**源码可见**许可，不是 OSI 定义的开源许可。商业授权请联系维护者。
