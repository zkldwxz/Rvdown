# Rvdown 下载对比与真实续传修复（2026-10-22）

已完成结果：基线 0.1.0（核心提交 `16ee138`）48 条受控公网/回环对比；续传修复 `123a05f` 后 24 条回环对比、三种工具的中断重启、Rust SDK 和 JSONL CLI 的真实 HLS/DASH 验证。

21.7MB 公开大视频另外按三轮执行中，已有超时；该组尚未完成，没有算通过。完成后更新本目录的最终报告。以下均为已经落盘的实际数据。

| 样本/口径 | Rvdown 8 路 MiB/s | aria2 8 路 MiB/s | httpx-fetch 等价单流 MiB/s |
|---|---:|---:|---:|
| 公网 MDN 小视频（基线） | 0.106 | 0.245 | 0.111 |
| 每连接限 4 MiB/s 回环（修复版） | 23.983 | 24.241 | 3.287 |
| 不限速回环（修复版） | 209.18 | 217.058 | 206.601 |

速度为三轮成功中位数；原始 JSON 保留全部失败、分母和耗时。FFmpeg 公开来源的 4.56MB 样本发生错误/超时，未产生合格速度值，12 条失败仍在原始基线中。公网小视频上 aria2 更快，不能声称 Rvdown 全场景领先。

公网三工具使用同一个本机 CONNECT 字节转发器，出站绑定物理网卡、使用同一公开 DoH 解析快照；TLS 端到端，不走外部代理、不改系统配置。转发器有公共开销。回环数据是合成字节，不是互联网下载速度。HTTPX 是独立实现的 httpx-fetch 等价基线（httpx 0.28.1 / 1MiB 流式 / 原子交付），不是 Windows PyInstaller 成品。字节对比关闭 Rvdown ffmpeg/MD5，三工具计时外核对 SHA256。

真实中断发现原版 `.part` 逻辑长度及资源域重复哈希两处问题。修复没有放宽强 ETag、尺寸或逐段落盘校验。修复版中断后复用 14 MiB，只重传 2 MiB，最终 SHA256 正确；旧版完整重下的原始结果保留。

- [修复版本地三轮与续传](fixed-local.json)
- [基线完整对比（含失败）](controlled-downloads.json)
- [修复版 SDK/CLI 真实 HLS/DASH：4/4](fixed-video-features.json)
- [修复版双任务 SDK/进度/并发](fixed-sdk-transfer.json)
- [源码/私人配置边界审查摘要](privacy-audit.json)
- [复现脚本与参数口径](../../../benchmarks/README.md)

核心修复后的 Rust 测试（含 doctest）169 通过，Python 基准/边界测试 7 通过，clippy/fmt 通过。测试机为 macOS arm64，一台 Mac 结果不代替 Windows 或所有 CDN 验收。这里只公开通用 SDK、基准脚本、公开数据/生成夹具的统计；没有应用代码、业务 API、私人配置、媒体、任务或原始日志。
