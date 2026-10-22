# Calling guide

Rvdown ships in two shapes with **identical fields and error codes**: a standalone executable (any
language) and a Rust SDK.

## 1. Executable

### 1.1 Single file

```bash
rvdown get <url> --out <path> [options]
```

| Flag | Meaning |
|---|---|
| `<url>` | Primary direct URL (`http`/`https`). HLS playlists are detected and remuxed automatically. |
| `--out, -o <path>` | Required output path; parent directories are created. |
| `--audio <url>` | Audio URL: presence makes it a DASH job (download both, then merge). |
| `--fallback <url>` | Fallback URL, repeatable; tried in order. |
| `--page-url <url>` | Page URL used to infer `Referer` when the target host matches no platform rule. |
| `--json` | Print one JSON result line instead of a human summary. |

Exit codes: `0` success · `1` failure · `2` bad arguments · `3` environment failure. On Ctrl-C
`get` is terminated by the signal (no result line; the shell reports `130`, and the `.part` file is
kept for resume).

### 1.2 Batch (recommended for scripts)

```bash
rvdown run --tasks <file|-> [--out <file>] [--progress] [--concurrency N] [shared options]
```

- `--tasks`: JSON Lines, one job per line; `-` reads stdin.
- `--out`: also write flat result lines (one object per line, no `event` field).
- `--progress`: emit throttled progress events (500 ms per job/asset at most).
- `--concurrency`: concurrent jobs (1-32, default 3).

Exit codes: `0` all succeeded · `1` some jobs failed (results complete) · `2` bad arguments or task
file (nothing executed) · `3` environment failure · `130` interrupted — unlike `get`, `run` catches
Ctrl-C, emits `CANCELLED` results for unfinished jobs and a `summary` with `"interrupted":true`.

stdout is the **protocol stream** (one JSON object per line); human-readable logs go to stderr.
Four event types: `start`, `progress` (only with `--progress`), `result`, `summary`.

```jsonc
{"event":"start","total":3}
{"event":"progress","id":"a","asset":"video","attempt":1,"phase":"downloading","validated_bytes":1048576,"resumed_bytes":0,"network_bytes":1048576,"total_bytes":5242880,"percent":20}
{"event":"result","id":"a","ok":true,"code":"OK","out":"/data/a.mp4","bytes":5242880,"md5":"…","retryable":false,"message":"download finished","attempts":3,"network_bytes":5242880,"resumed_bytes":0,"elapsed_ms":4210}
{"event":"summary","total":3,"ok":3,"failed":0,"interrupted":false}
```

- `progress.phase` is a stable machine code: `probing` · `downloading` · `merging` · `verifying`.
- `progress.total_bytes` and `progress.percent` are `null` when the server does not report a length
  (percentages are never faked).
- Optional result fields are omitted when absent; `warnings` is omitted when empty.

### 1.3 Shared options

| Flag | Default | Meaning |
|---|---|---|
| `--connections <N>` | 8 | Parallel segments per file (out-of-range values are clamped to 1-32). |
| `--retry-attempts <N>` | 3 | Attempts per request, including the first (minimum 1). |
| `--ffmpeg <path>` | auto | ffmpeg path. Default: `resources/ffmpeg[.exe]` next to the binary → `PATH`. Environment variables are **not** read; pass the path explicitly. |
| `--proxy <url>` | system | `http://` or `socks5://`; `--no-proxy` forces direct. |
| `--interface <name>` | none | Bind to a network interface; loopback targets stay local. Tunnel/loopback names are rejected. |
| `--dns-doh <url>` + `--dns-doh-ip <ip>` | system DNS | Resolve through DoH (the DoH request also uses `--interface`), bypassing tunnel fake-IP. Both flags required together. |
| `--header "Name: value"` | none | Global explicit header, repeatable; a job's own `headers` wins. |
| `--no-verify` | verify | Skip the ffmpeg probe (size check only). |
| `--no-checksum` | md5 | Do not compute the output MD5. |
| `--min-output-bytes <N>` | 16384 | Minimum plausible size; smaller outputs fail and are removed. |

## 2. Job fields (JSON Lines / `rvdown::Job`)

`rvdown::Job` is the serde mirror of this JSON — there is no second definition. A line that is not
valid JSON or contains an **unknown field** aborts the whole batch before anything runs (exit `2`),
so a typo fails loudly instead of being silently ignored. A well-formed job with invalid values
(empty `out`, non-HTTP `url`) yields an `INVALID_JOB` result and the rest of the batch continues.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | no | Caller identifier echoed back. Not used for de-duplication. |
| `url` | string | **yes** | Primary direct URL (`http`/`https`). |
| `urls` | string[] | no | Fallback URLs, tried in order; duplicates of `url` are ignored. |
| `audio_url` | string | no | Audio URL; presence means DASH. |
| `out` | string | **yes** | Output path including the file name. |
| `headers` | object | no | Explicit headers. **An empty string suppresses that header**; names are case-insensitive. |
| `page_url` | string | no | Page URL, used to infer `Referer` for unknown hosts. |
| `verify` | bool | no | Override the global verify setting for this job. |
| `checksum` | bool | no | Override the global MD5 setting for this job. |

## 3. Result fields (`rvdown::Outcome`)

| Field | Type | Meaning |
|---|---|---|
| `id` | string | Echoed job id. |
| `ok` | bool | Success flag. |
| `code` | string | **Stable error code** — see [error-codes.md](error-codes.md). Branch on this only. |
| `out` | string? | Output path (on success). |
| `bytes` | number? | Output size. |
| `md5` | string? | Output MD5 (absent when checksum is off). |
| `duration` | number? | Container duration in seconds (needs ffmpeg). |
| `has_video` / `has_audio` | bool? | ffmpeg probe results. |
| `source_url` | string? | The candidate URL that succeeded. |
| `http_status` | number? | HTTP status when the failure was an HTTP response. |
| `retryable` | bool | Trying the same job later **might** succeed (not a recommendation to retry now). |
| `message` | string | Fixed English sentence derived from `code`; do not parse. |
| `detail` | string? | Diagnostic text (may contain upstream wording). **Never store in business state.** |
| `attempts` | number | Attempts including the first. |
| `network_bytes` | number | Real network bytes (resumed bytes excluded). |
| `resumed_bytes` | number | Bytes restored from a partial download. |
| `elapsed_ms` | number | Wall-clock duration. |
| `warnings` | array? | `{code, message}`; e.g. `NO_AUDIO_TRACK`, `PROBE_SKIPPED`, `CHECKSUM_SKIPPED`. Omitted when empty. |

## 4. Rust SDK

```rust
let client = rvdown::Client::builder()
    .connections(16)
    .job_concurrency(4)
    .ffmpeg("/opt/ffmpeg")
    .build()?;

let outcome = client.run(&job).await;                   // one job

client.run_stream(&jobs, Some(&sink), &mut |outcome| {  // batch, callback per completion
    println!("{} {}", outcome.id, outcome.code.description());
}).await;

let outcomes = client.run_all(&jobs).await;             // batch, input order
```

Progress: implement `rvdown::ProgressSink` (`on_event` receives `TransferEvent` with `task_id`,
`asset_id`, `attempt_id`, `phase`, `validated_bytes`, `resumed_bytes`, `network_bytes`,
`total_bytes`; `percent()` returns `None` when the total is unknown — percentages are never faked).

Lower layers when you want to orchestrate yourself:

```rust
let engine = rvdown::Engine::new(config)?;
engine.fetch(url, dest, &headers, Some(page_url), None).await?;  // plain file (covers, audio)
engine.ffmpeg().probe(path).await?;                               // probe an output
```

## 5. Calling from Python / Node (examples)

Rvdown itself is pure Rust. `bindings/python/rvdown.py` and `bindings/node/rvdown.mjs` are optional
~100-line **calling examples**: they spawn the `rvdown` executable and parse JSON Lines. They contain
no download logic — the speed comes from the Rust binary — so you can copy them or call the
executable directly with `subprocess` / `child_process`.

```python
from rvdown import Rvdown

downloader = Rvdown("/usr/local/bin/rvdown")
for outcome in downloader.run([
    {"id": "a", "url": url, "out": "/data/a.mp4"},
]):
    print(outcome["id"], outcome["code"])
```

## 6. Compatibility

- Error codes are append-only; fields are append-only.
- Unknown job fields are **rejected** (the batch exits `2` before running) rather than ignored —
  check this document before upgrading.
- Pin versions: a specific release binary (with its SHA256), or `--tag vX.Y.Z` for the SDK.
