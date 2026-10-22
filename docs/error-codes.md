# 错误码 / Error codes

`code` 是跨语言契约，**只增不改语义**。`message` 由 `code` 派生（固定英文短句，即下表 English 列），
`detail` 只用于排查、可能含上游原始文案。`retryable` 列与结果里的 `retryable` 字段一致。

`code` is the cross-language contract: append-only, semantics never change. `message` is derived
from `code` (the fixed sentence in the English column); `detail` is diagnostic only and may contain
upstream wording. The retryable column matches the `retryable` result field.

| code | 中文 | English | retryable |
|---|---|---|---|
| `OK` | 下载完成（已按配置校验） | download finished | — |
| `OK_NO_AUDIO` | 下载完成，但成片没有音频轨（需人工确认） | download finished, but the output has no audio track | — |
| `INVALID_JOB` | 任务取值非法（url 为空或非 HTTP、路径为空）。整行不是 JSON 或含未知字段时整批退出 `2`，不产生此结果 | invalid job definition | 否 |
| `HTTP_NOT_FOUND` | 资源不存在（404/410） | resource not found (404/410) | 否 |
| `HTTP_FORBIDDEN` | 访问被拒（403：防盗链/频控/签名过期） | access denied (403) | 是（直链多半会刷新） |
| `HTTP_RATE_LIMITED` | 被限流（429） | rate limited (429) | 是 |
| `HTTP_SERVER_ERROR` | 服务端错误（5xx） | server error (5xx) | 是 |
| `HTTP_CLIENT_ERROR` | 其它 4xx | request rejected (4xx) | 否 |
| `NETWORK_ERROR` | 连接失败/重置/TLS 握手失败 | network error | 是 |
| `NETWORK_TIMEOUT` | 连接或读取超时 | network timeout | 是 |
| `PROTOCOL_ERROR` | 协议异常：区间不符、缺 `Content-Range`、服务器不支持分段且单流失败 | protocol error (range/content-range) | 是 |
| `RESOURCE_CHANGED` | 传输中资源变化，按有限重启重下后仍失败 | resource changed during transfer | 是 |
| `TARGET_BUSY` | 目标文件正被其它任务写入 | target file is locked by another task | 是 |
| `DISK_FULL` | 磁盘空间不足 | disk full | 否（永久） |
| `PERMISSION_DENIED` | 没有写入权限 | write permission denied | 否（永久） |
| `BAD_PATH` | 路径无效 | invalid path | 否（永久） |
| `IO_ERROR` | 其它文件系统错误 | filesystem error | 否（永久） |
| `FFMPEG_MISSING` | 找不到 ffmpeg 可执行文件 | ffmpeg executable not found | 否（装好 ffmpeg 后再跑） |
| `MERGE_FAILED` | ffmpeg 合并失败 | audio/video merge failed | 否 |
| `OUTPUT_TOO_SMALL` | 产物小于可信下限（疑似错误页） | output smaller than the plausible minimum | 否 |
| `OUTPUT_NO_VIDEO_TRACK` | 产物没有视频轨（可能是中断残件） | output has no video track | 否 |
| `OUTPUT_INVALID` | 产物为空或无法读取 | output is empty or unreadable | 否 |
| `CANCELLED` | 被中断，已完成部分保留、`.part` 可续传 | cancelled | 是 |

## 映射规则（实现细节，调用方不必依赖）

- HTTP 层失败按状态码细分：`404/410 → HTTP_NOT_FOUND`、`403 → HTTP_FORBIDDEN`、
  `429 → HTTP_RATE_LIMITED`、`5xx → HTTP_SERVER_ERROR`、其余 4xx → `HTTP_CLIENT_ERROR`。
- 传输层超时是 `NETWORK_TIMEOUT`；连接重置、DNS 失败、TLS 失败是 `NETWORK_ERROR`。
- 磁盘满 / 权限 / 路径 / 其它 IO 是**永久失败**：不会重试，候选回退也会立即停止。
- `retryable=true` 只表示「稍后重试**可能**成功」，不代表值得立刻重试；
  例如 `HTTP_FORBIDDEN` 通常需要调用方重新解析出新的签名直链。

## Warnings（`warnings[].code`）

| code | 含义 |
|---|---|
| `NO_AUDIO_TRACK` | 成片没有音频轨（单流源常见），下载仍算成功 |
| `PROBE_SKIPPED` | 没有 ffmpeg 或探测失败，只做了大小检查 |
| `CHECKSUM_SKIPPED` | MD5 计算失败 |
