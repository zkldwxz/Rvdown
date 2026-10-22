# Rvdown

[实测对比报告 / measured comparison](docs/benchmarks/2026-10-22/report.zh-CN.md) · [Reproduce](benchmarks/README.md)

Fast, multi-threaded video downloader for scripted pipelines — a drop-in replacement for `aria2`
in crawler / review / archiving jobs where CDNs fight back.

[中文说明](README.zh-CN.md) · [Calling guide](docs/calling.md) · [Error codes](docs/error-codes.md)

## Why another downloader

- **Segmented Range downloads** — N parallel byte ranges into a `.part` file, atomic rename on
  completion. Resumes across process restarts from a sidecar ledger (`.aria2` naming), and refuses
  to splice bytes from a resource that changed mid-flight (`ETag` / `Last-Modified` / length /
  redirect target).
- **Browser TLS fingerprint** — Douyin/TikTok and several domestic CDNs reject non-browser TLS
  stacks (aria2, Node `fetch`, Python `requests`) with `403`. Rvdown ships a Chrome 131 JA3 profile,
  so those links just work.
- **Automatic HLS** — a playlist URL is detected, segments are fetched in parallel, and the result is
  remuxed into a real media file. A playlist is never delivered as the output file.
- **Automatic DASH** — give `audio_url` and Rvdown downloads both streams and merges them with
  ffmpeg (stream copy). A missing audio stream is a hard failure, never a silent video.
- **Built-in Referer / UA matrix** — per-platform rules for bilibili / kuaishou / toutiao / douyin /
  xiaohongshu, re-evaluated on every redirect hop. Override any header per job; an empty value means
  "do not send it".
- **Output verification** — size floor plus ffmpeg stream probe. Error pages and truncated files are
  rejected and cleaned up instead of being reported as success.
- **Two shapes, one contract** — a Rust SDK and a single static binary speaking JSON Lines, with the
  exact same field names and error codes.

## Install

```bash
# Prebuilt binaries (Windows x64, macOS arm64/x64, Linux x64)
# https://github.com/zkldwxz/Rvdown/releases

# Rust SDK: pin a tag, not a branch
cargo add rvdown --git https://github.com/zkldwxz/Rvdown --tag v0.1.0
```

ffmpeg is optional and external: `ffmpeg` next to the binary under `resources/`, or on `PATH`, or
passed explicitly (`--ffmpeg` / `Config::ffmpeg`). Without it, DASH merging is unavailable and
verification degrades to a size check.

## CLI (any language)

```bash
rvdown get "https://cdn.example.com/video.mp4" --out ./video.mp4

# Batch: one job per line, results streamed back line by line
rvdown run --tasks jobs.jsonl --out results.jsonl --progress
```

```jsonc
// jobs.jsonl
{"id":"a","url":"https://cdn.example.com/v.mp4","out":"/data/a.mp4"}
{"id":"b","url":"https://cdn.example.com/main.m3u8","out":"/data/b.mp4","headers":{"Cookie":"s=1"}}
{"id":"c","url":"https://cdn.example.com/video.m4s","audio_url":"https://cdn.example.com/audio.m4s","out":"/data/c.mp4"}
{"id":"d","url":"https://cdn.example.com/v.mp4","urls":["https://mirror.example.com/v.mp4"],"out":"/data/d.mp4"}
```

Exit codes: `0` all succeeded · `1` some jobs failed (results are complete) · `2` bad arguments or
task file (nothing executed) · `3` environment failure · `130` interrupted (finished files kept,
`.part` resumable).

## Rust SDK

```rust
use rvdown::{Client, Job};

#[tokio::main]
async fn main() -> Result<(), rvdown::JobError> {
    let client = Client::builder()
        .connections(16)        // parallel segments per file
        .job_concurrency(4)     // files at once
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

Lower layers are public when you want to orchestrate yourself: `MediaDownloader` (file transfer with
HLS detection), `RangeDownloader` with a pluggable `SegmentDownloader`, and the ffmpeg adapters
(`AudioMuxer` / `VideoProbe` / `FrameGrabber`).

## Behavior worth knowing

- A job's own `headers` win over global `--header` values; an empty value suppresses the header
  entirely (useful for a CDN that rejects `Referer`).
- Progress bytes are split into *validated*, *resumed* and *network*: retries never make the
  percentage jump, and resume bytes never inflate the speed reading.
- Permanent filesystem errors (disk full, permission, bad path) fail immediately; transient HTTP
  failures (403/408/425/429/5xx, network resets) are retried with `Retry-After` first, then
  exponential backoff.
- A file currently being written by another job fails with `TARGET_BUSY` rather than racing it.

## License

CC BY-NC-SA 4.0 — non-commercial use only; derivatives must be distributed under the same license.
See [LICENSE](LICENSE). This is a *source-available* license, not an OSI-approved open source one.
For commercial licensing, contact the maintainer.
