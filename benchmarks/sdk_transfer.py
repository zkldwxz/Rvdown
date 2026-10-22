"""Measure two concurrent Rust SDK jobs against a deterministic loopback fixture."""
import argparse
import hashlib
import json
import pathlib
import threading

from compare import RangeFixture, digest, measure, MIB


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sdk", required=True)
    parser.add_argument("--work", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.timeout = 30
    args.work.mkdir(parents=True)
    with RangeFixture() as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        expected = hashlib.sha256(server.data).hexdigest()
        jobs = [{"id": f"job-{n}", "url": f"http://127.0.0.1:{server.server_port}/bytes",
                 "out": str(args.work / f"job-{n}.bin"), "verify": False}
                for n in range(2)]
        tasks = args.work / "jobs.jsonl"
        tasks.write_text("\n".join(json.dumps(job) for job in jobs))
        measured, events = measure([args.sdk, str(tasks)], args, "sdk")
        verified = all(pathlib.Path(job["out"]).exists() and digest(job["out"]) == expected for job in jobs)
        progress = [event for event in events if event.get("event") == "progress"]
        monotonic = True
        previous = {}
        for event in progress:
            key = (event["id"], event["asset"], event["attempt"])
            pair = (event["validated_bytes"], event["network_bytes"])
            old = previous.get(key, (0, 0))
            monotonic &= pair[0] >= old[0] and pair[1] >= old[1]
            previous[key] = pair
        bounded = all(event["total_bytes"] is None or 0 <= event["validated_bytes"] <= event["total_bytes"] for event in progress)
        results = [event["outcome"] for event in events if event.get("event") == "result"]
        ok = measured["exit_code"] == 0 and verified and len(results) == 2 and all(row["ok"] for row in results) and monotonic and bounded
        report = {"schema": 1, "kind": "rust_sdk_two_job_capped_loopback", "ok": bool(ok),
                  "jobs": 2, "bytes_per_job": len(server.data), "connections_per_job": 8,
                  "per_connection_cap_mib_s": 4, "measurement": measured,
                  "aggregate_mib_s": round(2 * len(server.data) / MIB / measured["elapsed_s"], 3),
                  "sha256_verified": verified, "expected_sha256": expected,
                  "progress_events": len(progress), "progress_monotonic": bool(monotonic),
                  "progress_bounded": bounded, "task_ids_observed": sorted({event["id"] for event in progress}),
                  "server": server.stats(), "sdk_executable_sha256": digest(args.sdk),
                  "note": "non-video fixture; MD5 enabled, ffmpeg disabled per job; aggregate is not WAN speed"}
        server.shutdown()
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    if not ok:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
