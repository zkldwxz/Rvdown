"""Actual external ffmpeg + loopback HLS/DASH + Rust SDK/JSONL integration.

All media are locally generated testsrc/sine. Reports contain only explicit facts,
never raw jobs, paths, host addresses, environment or subprocess logs.
"""
import argparse
import contextlib
import hashlib
import json
import pathlib
import subprocess
import threading

from compare import RangeFixture, clean_environment, digest


def command(args):
    return subprocess.run(args, capture_output=True, text=True,
                          env=clean_environment(), timeout=90)


def probe(file):
    result = command(["ffprobe", "-v", "error", "-show_entries", "stream=codec_type:format=duration",
                      "-of", "json", str(file)])
    if result.returncode:
        return {}
    value = json.loads(result.stdout)
    kinds = {stream["codec_type"] for stream in value.get("streams", [])}
    return {"duration_s": float(value.get("format", {}).get("duration", 0)),
            "video": "video" in kinds, "audio": "audio" in kinds}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rvdown", required=True)
    parser.add_argument("--sdk", required=True)
    parser.add_argument("--work", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.work.mkdir(parents=True)
    source = args.work / "generated.mp4"
    generated = command(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=24",
        "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100", "-t", "4", "-c:v", "libx264",
        "-preset", "ultrafast", "-g", "24", "-c:a", "aac", "-movflags", "+faststart", str(source)])
    if generated.returncode:
        raise RuntimeError("fixture generation failed")
    media = args.work / "source"
    media.mkdir()
    for name, maps in [("video.mp4", ["-an"]), ("audio.m4a", ["-vn"])]:
        result = command(["ffmpeg", "-v", "error", "-i", str(source), *maps, "-c", "copy", str(media / name)])
        if result.returncode:
            raise RuntimeError("DASH fixture generation failed")
    result = command(["ffmpeg", "-v", "error", "-i", str(source), "-c", "copy", "-hls_time", "1",
                      "-hls_list_size", "0", "-hls_segment_filename", str(media / "chunk-%03d.ts"),
                      str(media / "index.m3u8")])
    if result.returncode:
        raise RuntimeError("HLS fixture generation failed")
    report = {"schema": 1, "kind": "generated_media_loopback_integration", "cases": [],
              "ffmpeg": command(["ffmpeg", "-version"]).stdout.splitlines()[0],
              "sdk_executable": {"sha256": digest(args.sdk), "bytes": pathlib.Path(args.sdk).stat().st_size}}
    with RangeFixture() as server:
        server.throttled = False
        server.media = media
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_port}"
        jobs = [{"id": "hls", "url": base + "/index.m3u8", "out": str(args.work / "hls.mp4")},
                {"id": "dash", "url": base + "/video.mp4", "audio_url": base + "/audio.m4a",
                 "out": str(args.work / "dash.mp4")}]
        for interface in ["sdk", "cli"]:
            selected = [{**job, "out": str(args.work / (interface + "-" + job["id"] + ".mp4"))} for job in jobs]
            task = args.work / (interface + ".jsonl")
            task.write_text("\n".join(json.dumps(job) for job in selected))
            invocation = [args.sdk, str(task)] if interface == "sdk" else [args.rvdown, "run", "--tasks",
                str(task), "--progress", "--no-proxy", "--concurrency", "2"]
            result = command(invocation)
            (args.work / (interface + ".stdout")).write_text(result.stdout)
            (args.work / (interface + ".stderr")).write_text(result.stderr)
            events = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
            progress = [event for event in events if event.get("event") == "progress"]
            for job in selected:
                file = pathlib.Path(job["out"])
                facts = probe(file) if file.exists() else {}
                relevant = [event for event in progress if event.get("id") == job["id"]]
                byte_bounds = all(event.get("total_bytes") is None or
                                  0 <= event["validated_bytes"] <= event["total_bytes"] for event in relevant)
                percentages = all(event.get("percent") is None or 0 <= event["percent"] <= 100 for event in relevant)
                ok = (result.returncode == 0 and facts.get("video") and facts.get("audio")
                      and 3.5 < facts.get("duration_s", 0) < 4.5 and bool(relevant)
                      and byte_bounds and percentages)
                report["cases"].append({"interface": interface, "case": job["id"], "ok": bool(ok),
                    "exit_code": result.returncode, **facts, "progress_event_count": len(relevant),
                    "progress_byte_bounds": byte_bounds, "progress_percent_bounds": percentages,
                    "bytes": file.stat().st_size if file.exists() else 0,
                    "sha256": digest(file) if file.exists() else None})
        # SDK concurrency uses two distinct jobs; keep the controlled request peak as evidence.
        report["server_peak_concurrent_requests"] = server.max_active
        server.shutdown()
    report["passed"] = sum(case["ok"] for case in report["cases"])
    report["attempted"] = len(report["cases"])
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    if report["passed"] != report["attempted"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
