# Rvdown · contributor and agent instructions

## Purpose

Rvdown means **Rust + Video + Download**: pursue high performance, small resource use,
fast transfers and safety in Rust; make video downloading convenient, easy to integrate
and fast; retain compatibility as a general downloader. These are engineering goals,
not a claim that Rust or Rvdown always beats another engine.

Video delivery is the primary evaluation. Keep improving general transfer speed, but
judge the product first by real video success, usable-file delivery time, recovery,
resource use and SDK/CLI integration. Follow the [video benchmark plan](docs/video-benchmark-plan.zh-CN.md).

## Start here when resuming work

- Read this file, the README, the benchmark plan and the latest relevant report. Check
  Git HEAD, dirty files and actual evidence before inferring what is complete. Preserve
  unrelated local changes; update the plan with results and unfinished work.
- Keep work consolidated. Do not create Orca tasks, other worker branches or subagents
  unless the human explicitly requests delegation.
- Historical MDN measurements are auxiliary diagnostics. They do not establish Douyin
  CDN performance or a universal aria2 ranking. A single successful video is not a full
  comparative benchmark. Never turn a goal, fixture or skipped case into a claimed pass.

## Repository boundary and safety

- Keep the Rust SDK and CLI on one shared download core. Application-specific parsing,
  authentication, business APIs and workflow state belong to callers, outside this repo.
  The library accepts authorized direct URLs and headers; it must build independently.
- Never commit private configuration, signed URLs, cookies/tokens, user/business IDs,
  internal hosts/IPs, environment snapshots, raw private logs, media or sibling-repo
  code. Private runtime inputs stay in memory or private temporary storage. Publish
  only reviewed, allowlisted aggregates; inspect both the diff and reachable Git history.
- Public reproduction fixtures use public, credential-free inputs or generated media.
  Real platform CDN testing uses separately authorized runtime inputs. Do not add a
  private resolver as a build/test dependency or hide private inputs in a public script.
- Preserve resource identity, validator/Range checks, safe resume, bounded retry,
  permanent filesystem failure handling, target ownership and atomic publication.
  Never trade those guards for a faster benchmark. No partial file, error page,
  playlist or required-audio-missing output may be reported as successful video delivery.
- ffmpeg remains external; count its process/dependency costs in end-to-end scenarios.
  Keep the existing license; source visibility does not imply an OSI-approved license.

## Benchmark priorities and fairness

1. Use **actual Douyin video CDN URLs** as the primary comparison with aria2 and
   httpx-fetch. Expand to other real video CDNs, HLS and DASH. MDN/public samples,
   GitHub mirrors and loopback fixtures are separately labelled diagnostics, never
   substitutes for this primary video evidence.
2. Start with one video at a time, fresh equivalent URLs/required headers, the same
   source/IP/physical egress and comparable connection budgets. Rotate tool order and
   repeat trials; record actual connections/protocols as well as configured limits.
   Separate single-connection, matched-budget and each-tool-default results. Batch/SDK
   scenarios are separate. Do not handicap competitors by removing necessary headers.
3. Compare pure transfer and usable-video delivery separately. The latter includes
   verification and any required HLS/DASH/ffmpeg orchestration for every tool. Header
   omission, forced disconnect, fingerprint or protocol changes are separate controls;
   preserve original authorized URLs and distinguish HTTP headers from TLS behavior.
4. Retain every planned failure, timeout and invalid output in the denominator. Report
   success counts alongside median/range and success-only speeds. Split size/protocol/
   platform groups; do not claim representative tail percentiles from a tiny sample.
   Include CPU, peak memory, network/repeated bytes and binary/runtime footprint.
5. Bind results to source commit, clean/dirty state, exact binaries/hashes, versions,
   timing boundaries, sample manifest digest and egress evidence. Clearing proxy env
   alone cannot exclude a TUN proxy. Do not change OS proxy settings without consent.
   Separate harness/setup failures from CDN responses; neither counts as a pass.
6. Diagnose before changing defaults. HEAD removal, more connections, fingerprint
   rotation and protocol forcing need evidence on the target video workload. Author
   claims and local simulations are not our real-CDN performance results.

## Change validation

New behavior starts with a failing test of the invariant, then the smallest change.
Default tests must be deterministic and offline; actual CDN runs are explicit,
authorized and separately reported. Run:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --all --check
python3 -m unittest discover -s benchmarks -p 'test_*.py'
cargo test --locked -p rvdown --example reconnect_probe
cargo test --locked -p rvdown --example request_probe
```

Python benchmark tests require Python 3.11+ and the dependencies documented in
`benchmarks/README.md`. Source/platform changes invalidate affected prior evidence;
macOS or CI builds alone do not prove Windows runtime/media acceptance. Preserve
failure evidence until diagnosis is complete, then remove temporary media. Keep
sanitized reports and state remaining gaps plainly. Do not publish raw run folders.
