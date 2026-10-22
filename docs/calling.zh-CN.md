# 调用文档

Rvdown 只有两种调用形态，**字段与错误码完全一致**：可执行文件（任何语言）与 Rust SDK。

## 1. 可执行文件

### 1.1 单文件

```bash
rvdown get <url> --out <path> [选项]
```

| 参数 | 说明 |
|---|---|
| `<url>` | 主直链（HTTP/HTTPS）。HLS 播放列表会自动识别并封装。 |
| `--out, -o <path>` | 必填，产物路径（含文件名），父目录自动创建。 |
| `--audio <url>` | 音频直链：给了就是 DASH 双流任务，下完再合并。 |
| `--fallback <url>` | 备用直链，可重复；主地址失败按顺序回退。 |
| `--page-url <url>` | 原始内容页 URL：目标域名识别不出平台时用它的 origin 兜底 Referer。 |
| `--json` | 输出一行 JSON（与批量结果字段相同）而不是人类可读摘要。 |

退出码：成功 `0`，失败 `1`，参数错误 `2`，环境错误 `3`。`get` 不接管 Ctrl-C：被中断时进程直接终止，
不输出结果行（shell 报 `130`），`.part` 保留可续传。

### 1.2 批量（推荐给脚本）

```bash
rvdown run --tasks <file|-> [--out <file>] [--progress] [--concurrency N] [共享选项]
```

- `--tasks`：JSON Lines 文件，一行一个任务；`-` 表示从 stdin 读。
- `--out`：另写一份**纯结果行**（每行一个结果对象，无 `event` 字段），便于事后处理。
- `--progress`：额外输出节流进度事件（同一任务同一资产 500ms 一条）。
- `--concurrency`：同时下载的任务数（1-32，默认 3）。

退出码：`0` 全部成功 · `1` 有任务失败（结果完整）· `2` 参数或任务文件错误（未执行）· `3` 环境错误 ·
`130` 被中断——与 `get` 不同，`run` 会接管 Ctrl-C，给未完成任务补 `CANCELLED` 结果，并输出
`"interrupted":true` 的 `summary`。

stdout 是**协议流**（一行一个 JSON 对象），人类可读日志只走 stderr。四种事件：`start`、
`progress`（仅 `--progress`）、`result`、`summary`。

```jsonc
{"event":"start","total":3}
{"event":"progress","id":"a","asset":"video","attempt":1,"phase":"downloading","validated_bytes":1048576,"resumed_bytes":0,"network_bytes":1048576,"total_bytes":5242880,"percent":20}
{"event":"result","id":"a","ok":true,"code":"OK","out":"/data/a.mp4","bytes":5242880,"md5":"…","retryable":false,"message":"download finished","attempts":3,"network_bytes":5242880,"resumed_bytes":0,"elapsed_ms":4210}
{"event":"summary","total":3,"ok":3,"failed":0,"interrupted":false}
```

- `progress.phase` 是稳定机器码：`probing` · `downloading` · `merging` · `verifying`。
- 服务器不给长度时 `progress.total_bytes` 与 `progress.percent` 为 `null`（不伪造百分比）。
- 结果里的可选字段缺省时不输出；`warnings` 为空时不输出。

### 1.3 共享选项

| 选项 | 默认 | 说明 |
|---|---|---|
| `--connections <N>` | 8 | 单文件并行分段数（越界自动夹到 1-32）。 |
| `--retry-attempts <N>` | 3 | 单次请求尝试总数（含首次，最少 1）。 |
| `--ffmpeg <path>` | 自动 | ffmpeg 路径。默认：同目录 `resources/ffmpeg[.exe]` → `PATH`。**不读环境变量**，需要时显式传入。 |
| `--proxy <url>` | 系统代理 | `http://` 或 `socks5://`；`--no-proxy` 强制直连。 |
| `--interface <name>` | 无 | 绑定物理网卡；回环地址不绑定。隧道/回环网卡名会被拒绝。 |
| `--dns-doh <url>` + `--dns-doh-ip <ip>` | 系统 DNS | 用 DoH 解析（请求同样绑定 `--interface`），绕开隧道 fake-IP。两者必须同时给。 |
| `--header "名: 值"` | 无 | 全局显式头，可重复；任务自身 `headers` 优先。 |
| `--no-verify` | 校验 | 跳过 ffmpeg 流信息校验（只查大小）。 |
| `--no-checksum` | 算 MD5 | 不计算产物 MD5。 |
| `--min-output-bytes <N>` | 16384 | 产物可信下限，小于即判失败并清理。 |

## 2. 任务字段（JSON Lines / `rvdown::Job`）

Rust 侧的 `rvdown::Job` 就是这个 JSON 的 serde 同构类型；**两种形态没有第二份定义**。
某行不是合法 JSON 或含**未知字段**时，整批在执行前就拒绝（退出码 `2`），防止拼错字段名被静默忽略。
格式合法但取值非法的任务（`out` 为空、`url` 不是 HTTP）得到 `INVALID_JOB` 结果，其余任务照常执行。

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string | 否 | 调用方标识，原样回传；为空时回传空串。可重复，不参与去重。 |
| `url` | string | **是** | 主直链，必须是 `http`/`https`。 |
| `urls` | string[] | 否 | 备用直链，按顺序回退；与 `url` 重复的自动忽略。 |
| `audio_url` | string | 否 | 音频直链；给出即按 DASH 处理。 |
| `out` | string | **是** | 产物路径（含文件名）。 |
| `headers` | object | 否 | 显式请求头。**空串 = 明确不发送该头**；头名大小写无关。 |
| `page_url` | string | 否 | 原始内容页 URL，用于兜底推断 Referer。 |
| `verify` | bool | 否 | 覆盖全局是否校验产物。 |
| `checksum` | bool | 否 | 覆盖全局是否计算 MD5。 |

## 3. 结果字段（`rvdown::Outcome`）

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | 原样回传任务 id。 |
| `ok` | bool | 是否成功。 |
| `code` | string | **稳定错误码**，见 [error-codes.md](error-codes.md)。调用方只准按它分支。 |
| `out` | string? | 产物绝对/相对路径（成功时）。 |
| `bytes` | number? | 产物字节数。 |
| `md5` | string? | 产物 MD5（关闭 checksum 时缺省）。 |
| `duration` | number? | 容器内真实时长（秒），需要 ffmpeg。 |
| `has_video` / `has_audio` | bool? | ffmpeg 探测结果。 |
| `source_url` | string? | 最终成功的那条候选地址。 |
| `http_status` | number? | HTTP 层失败时的状态码。 |
| `retryable` | bool | 稍后重试同一任务是否**可能**成功（不代表值得立刻重试）。 |
| `message` | string | 由错误码决定的固定英文短句，不必解析。 |
| `detail` | string? | 排查原文（可能含上游文案）。**不要写入业务状态**。 |
| `attempts` | number | 尝试次数（含首次）。 |
| `network_bytes` | number | 本次真实网络字节（断点恢复不计入）。 |
| `resumed_bytes` | number | 断点恢复的字节。 |
| `elapsed_ms` | number | 耗时。 |
| `warnings` | array? | `{code, message}`；例如 `NO_AUDIO_TRACK`、`PROBE_SKIPPED`、`CHECKSUM_SKIPPED`。为空时不输出。 |

## 4. Rust SDK

```rust
let client = rvdown::Client::builder()
    .connections(16)
    .job_concurrency(4)
    .ffmpeg("/opt/ffmpeg")
    .build()?;

// 单条
let outcome = client.run(&job).await;

// 批量，完成一条回调一条
client.run_stream(&jobs, Some(&sink), &mut |outcome| {
    println!("{} {}", outcome.id, outcome.code.description());
}).await;

// 批量并按输入顺序收集
let outcomes = client.run_all(&jobs).await;
```

进度：实现 `rvdown::ProgressSink`（`on_event` 拿 `TransferEvent`）。字段：
`task_id` / `asset_id` / `attempt_id` / `phase` / `validated_bytes` / `resumed_bytes` /
`network_bytes` / `total_bytes`，`percent()` 在总量未知时返回 `None`（**不伪造百分比**）。

需要更低层时：

```rust
let engine = rvdown::Engine::new(config)?;
engine.fetch(url, dest, &headers, Some(page_url), None).await?;   // 纯文件下载（封面等）
engine.ffmpeg().probe(path).await?;                                // 产物探测
```

## 5. 从 Python / Node 调用（示例）

Rvdown 本身是纯 Rust。`bindings/python/rvdown.py` 与 `bindings/node/rvdown.mjs` 是可选的百行级
**调用示例**：启动 `rvdown` 可执行文件、解析 JSON Lines。里面没有下载逻辑——速度来自 Rust 二进制，
可以直接拷走，也可以自己用 `subprocess` / `child_process` 调可执行文件。

```python
from rvdown import Rvdown

downloader = Rvdown("/usr/local/bin/rvdown")
for outcome in downloader.run([
    {"id": "a", "url": url, "out": "/data/a.mp4"},
]):
    print(outcome["id"], outcome["code"])
```

## 6. 升级与兼容

- 错误码只增不改语义；新字段只追加。
- 任务里出现未知字段会被**拒绝**（整批退出码 `2`，未执行），不会静默忽略——升级前先对照本文档。
- 调用方应钉版本：CLI 用具体 release 的二进制（附 SHA256），SDK 用 `--tag vX.Y.Z`。
