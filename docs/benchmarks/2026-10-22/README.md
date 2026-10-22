# Downloader measurements — 2026-10-22

[中文报告](report.zh-CN.md) · [Reproduction](../../../benchmarks/README.md)

Earlier diagnostics cover one file at a time, single-connection versus segmented trials,
actual request traces, fixed resume, real ffmpeg HLS/DASH and concurrent Rust SDK progress.
Incomplete large-file WAN trials remain explicitly incomplete. One Mac is not Windows runtime
acceptance. The newer real-CDN run uses private inputs outside this repository and publishes
only an allowlisted aggregate; no application sources, private configuration or media are stored here.

Public samples and fixtures remain auxiliary diagnostics. A final-code real-CDN pure-transfer
matrix now compares Rvdown, aria2, and httpx-fetch across 15 video samples (300/300 complete
downloads); the [adaptive Douyin report](adaptive-segments.zh-CN.md) supports only the narrow
Douyin <=32 MiB candidate and does not show that Rvdown is generally faster than aria2. Full
HLS/DASH delivery, more independent samples, native-path confirmation, and Windows runtime
acceptance remain open. See the [video-first benchmark plan](../../video-benchmark-plan.zh-CN.md).
