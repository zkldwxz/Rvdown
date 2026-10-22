# Downloader measurements — 2026-10-22

[中文报告](report.zh-CN.md) · [Reproduction](../../../benchmarks/README.md)

One file at a time; balanced single-connection versus segmented small-video trials,
actual request traces, original three-tool comparison including failures, fixed resume,
real ffmpeg HLS/DASH and concurrent Rust SDK progress. Incomplete large-file WAN trials
remain explicitly incomplete. One Mac is not Windows runtime acceptance. Public inputs
and generated fixtures only; no application sources, private configuration or media.

These public samples and fixtures are auxiliary diagnostics. The primary product evaluation
will use actual Douyin video CDNs; its multi-sample three-tool benchmark is not complete.
See the [video-first benchmark plan](../../video-benchmark-plan.zh-CN.md).
