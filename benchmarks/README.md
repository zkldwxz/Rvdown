# Reproducible downloader comparison

This directory is independent of any application that embeds Rvdown. It takes public
HTTPS video URLs or generates local fixtures. It does not read application configuration,
credentials, content IDs, business APIs, spreadsheets, cookies or environment snapshots.

## Prerequisites

- Rust stable, external ffmpeg/ffprobe, aria2 1.37.0, Python 3.12+.
- A Python environment containing **httpx 0.28.1**.
- macOS or Linux for explicit physical-interface binding. Peak RSS and per-process CPU
  accounting currently use macOS `/usr/bin/time -l`; other OSes leave those fields null.
- Optional `curl` for an explicitly selected DNS-over-HTTPS provider.

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all --check
python3 -m unittest discover -s benchmarks -p 'test_*.py'
cargo build --locked --release
cargo build --locked --release --example verify_sdk
```

## Transfer speed, memory and resume

Use a new scratch directory on each run. Raw output and media are private scratch data;
only the constructed report JSON is suitable for review before publication.

```sh
python3 benchmarks/compare.py \
  --rvdown target/release/rvdown --aria2 aria2c --python python3 \
  --interface YOUR_PHYSICAL_INTERFACE \
  --rounds 3 --timeout 60 --mode all \
  --work /tmp/rvdown-comparison-new --report /tmp/rvdown-comparison.json
```

`--samples public-samples.json` accepts `[id, HTTPS_URL]` pairs. Credentials, query strings,
fragments and literal private/fake IPs are rejected. Example public samples:

```json
[
  ["mdn-flower", "https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4"],
  ["ffmpeg-4m", "https://samples.ffmpeg.org/MPEG-4/mp4video_mp3audio.mp4"]
]
```

- The three main tools rotate order across three rounds. Rvdown/aria2 use 8 connections,
  1 MiB minimum splits and one attempt. The HTTPX baseline streams 1 MiB chunks from one
  GET, follows redirects, ignores proxy variables, and atomically renames `.part` on success.
  It reproduces **httpx-fetch-style transfer behavior**, not a Windows packaged executable.
  Its interface binding and fixed DNS map are benchmark controls, not speed optimizations.
- A separate Rvdown single-connection sensitivity series runs after the balanced trials;
  its position is not balanced against the other tools. Do not treat it as a fourth tool
  in the Latin-square comparison.
- Transfer timing includes process startup, DNS/TLS and atomic delivery. To compare equivalent
  byte transfers, Rvdown's extra ffmpeg probe and MD5 are disabled in this series; SHA256 is
  checked for every tool outside the timer. The media integration below uses full verification.
- The reference hash comes from a one-time Rvdown preflight. It establishes byte equality,
  not independent provenance; the full expected size/hash is included in the report. Even if
  preflight fails, **every scheduled trial still runs** and remains in the denominator.
- WAN trials resolve each approved host once through physical-interface-bound public DoH.
  All three tools then use the **same benchmark-owned loopback CONNECT byte relay**, whose
  upstream sockets bind the physical interface and connect to that fixed public IP. TLS
  stays end-to-end (no certificate replacement). This avoids OS DNS / fake-IP interception,
  including interception of aria2's UDP DNS. It uses no external proxy and changes no system
  settings. The shared Python relay adds copy/CPU overhead; these are controlled-path WAN
  observations, not a claim about each tool's maximum native performance.
- Capped loopback tests use deterministic **non-video** bytes and a per-connection speed cap.
  Uncapped loopback throughput measures process/disk/loopback overhead. Neither is WAN speed.
- Resume tests interrupt a real process, retain its files, restart the same command, check
  the final SHA256 and count bytes served on the second attempt. Server application bytes
  are not packet-capture wire bytes. The intentionally slow final segment gives other
  segments time to finish before interruption. HTTPX is expected to restart from zero.
- macOS peak RSS is a process measurement, not a dependency footprint or an SDK heap estimate.
  Executable bytes alone exclude Python packages, dynamic libraries and external ffmpeg.
  Do not compare the Python launcher size to a complete standalone executable.
- Failed/time-out trials have no speed score. Summaries retain attempted/passed/failed and
  explicitly label success-only medians. No claim of universal superiority follows from them.

## Actual video integration and Rust SDK

```sh
python3 benchmarks/video_features.py \
  --rvdown target/release/rvdown --sdk target/release/examples/verify_sdk \
  --work /tmp/rvdown-generated-video-new --report /tmp/rvdown-generated-video.json
```

The script creates its own testsrc/sine video, produces HLS and split DASH streams with
real ffmpeg, then downloads them through both the Rust SDK and JSONL CLI. It checks actual
video/audio tracks, duration and progress bounds. The SDK example runs two jobs concurrently.
Generated fixtures and raw logs are never committed. These checks do not replace Windows
runtime validation or a real CDN compatibility test.

For two concurrent SDK byte-transfer jobs with progress assertions:

```sh
python3 benchmarks/sdk_transfer.py --sdk target/release/examples/verify_sdk \
  --work /tmp/rvdown-sdk-new --report /tmp/rvdown-sdk.json
```

This uses two 16 MiB deterministic streams, 8 connections per job and a 4 MiB/s per-connection
loopback cap. It checks SHA256, per-asset monotonic/bounded progress and observes actual server
connection concurrency. MD5 stays enabled, media probing is disabled for these non-video bytes.

## Publishing a report

Review the explicit report fields and all source differences. Do not copy raw logs, generated
task files, DNS maps, media, environment dumps or application data into this repository.
Record the library revision, exact binary SHA256 and harness hashes. State host architecture,
sample sizes, failures, network controls and measurement limitations alongside any speed table.
