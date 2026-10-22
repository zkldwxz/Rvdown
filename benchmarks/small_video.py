#!/usr/bin/env python3
"""Balanced one-file-at-a-time small-video experiment, with observed request traces."""
import argparse
import hashlib
import json
import pathlib
import platform
import shutil
import statistics
import subprocess
import threading
import time
import urllib.parse

import compare

PROFILES = ["rvdown-1", "rvdown-8", "aria2-default-threshold", "aria2-1m-threshold"]
URL = "https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4"
KNOWN_SHA256 = "0cd83d944a6ca7822b4a8306cecc60a36e859b041f6702c6a1ad9ead78924451"


def order(number):
    return compare.round_order(PROFILES, number)


def command(profile, url, dest, args, local):
    tool = profile if profile.startswith("rvdown") else "aria2-8"
    result = compare.command_for(tool, url, dest, args, local)
    if profile == "aria2-default-threshold":
        result[result.index("--min-split-size=1M")] = "--min-split-size=20M"
    return result


def summary(rows, planned):
    ok = [row for row in rows if row["ok"]]
    values = [row["elapsed_s"] for row in ok]
    return {"planned":planned, "attempted":len(rows), "passed":len(ok),
            "failed":len(rows)-len(ok), "uncompleted":planned-len(rows),
            "median_elapsed_s":statistics.median(values) if values else None,
            "min_elapsed_s":min(values) if values else None,
            "max_elapsed_s":max(values) if values else None}


class TraceHandler(compare.FixtureHandler):
    def serve(self, body):
        with self.server.lock:
            self.server.trace.append({"method":self.command, "range":self.headers.get("Range")})
        super().serve(body)


class TraceFixture(compare.RangeFixture):
    def __init__(self, data, throttled):
        super().__init__()
        self.RequestHandlerClass = TraceHandler
        self.data = data
        self.throttled = throttled
        self.trace = []

    def reset(self):
        # A deliberately aborted probe can leave a server write briefly in progress.
        limit = time.monotonic() + 5
        while self.active and time.monotonic() < limit:
            time.sleep(0.01)
        if self.active:
            raise RuntimeError("previous request did not finish; refuse mixed trial counters")
        super().reset()
        with self.lock:
            self.trace = []

    def stats(self):
        result = super().stats()
        with self.lock:
            result["request_trace"] = list(self.trace)
        return result


def generate(args, label, bitrate):
    dest = args.work / (label + ".mp4")
    parameters = ["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i",
                  "testsrc2=size=320x180:rate=30", "-f", "lavfi", "-i",
                  "sine=frequency=440:sample_rate=44100", "-t", "6", "-c:v", "libx264",
                  "-threads", "1", "-preset", "ultrafast", "-b:v", f"{bitrate}k",
                  "-minrate", f"{bitrate}k", "-maxrate", f"{bitrate}k", "-bufsize", f"{bitrate}k",
                  "-x264-params", "nal-hrd=cbr:force-cfr=1", "-c:a", "aac", "-b:a", "96k",
                  "-movflags", "+faststart", "-y", str(dest)]
    subprocess.run([args.ffmpeg, *parameters], env=compare.clean_environment(), check=True)
    probe = json.loads(compare.output([args.ffprobe, "-v", "error", "-show_streams", "-show_format", "-of", "json", str(dest)]))
    tracks = [row["codec_type"] for row in probe["streams"]]
    if "video" not in tracks or "audio" not in tracks:
        raise RuntimeError("generated fixture is not an audio/video MP4")
    return dest, {"id":label, "bytes":dest.stat().st_size, "sha256":compare.digest(dest),
                  "duration_s":float(probe["format"]["duration"]), "tracks":tracks,
                  "generation":"ffmpeg testsrc2 + sine, 6 s, libx264 CBR, AAC; no external input",
                  "video_bitrate_kbit_s":bitrate}


def series(args, report, label, url, expected, server=None, relay=None):
    for number in range(args.rounds):
        for profile in order(number):
            dest = args.work / f"{label}-{number}-{profile}.mp4"
            if server:
                server.reset()
            before = relay.connections if relay else 0
            measured, _ = compare.measure(command(profile, url, dest, args, bool(server)), args,
                                           f"{label}-{number}-{profile}")
            size = dest.stat().st_size if dest.exists() else 0
            actual = compare.digest(dest) if size else None
            row = {"sample":label, "round":number+1, "profile":profile, **measured,
                   "bytes":size, "sha256":actual,
                   "ok":compare.valid_result(measured["exit_code"], size, actual, expected)}
            if server:
                row["server"] = server.stats()
            if relay:
                row["upstream_tcp_connections"] = relay.connections - before
            report["runs"].append(row)
            compare.save(report, args)
            print(json.dumps(row), flush=True)
            if row["ok"]:
                dest.unlink()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rvdown", required=True)
    parser.add_argument("--aria2", default="aria2c")
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--ffprobe", default="ffprobe")
    parser.add_argument("--work", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--interface", required=True)
    parser.add_argument("--rounds", type=int, default=4)
    parser.add_argument("--timeout", type=float, default=45)
    parser.add_argument("--mode", choices=("all", "local", "wan"), default="all")
    parser.add_argument("--doh", default="https://dns.alidns.com/resolve")
    parser.add_argument("--doh-ip", default="223.5.5.5")
    args = parser.parse_args()
    if args.rounds < 4 or args.rounds % 4:
        parser.error("rounds must be a positive multiple of four, at least four")
    if not args.interface.startswith(("en", "eth", "wlan", "wl")):
        parser.error("choose an explicit physical interface")
    if args.work.exists() and any(args.work.iterdir()):
        parser.error("work directory must be empty")
    args.work.mkdir(parents=True, exist_ok=True)
    report = {"schema":1, "date":time.strftime("%Y-%m-%d", time.gmtime()),
              "source_commit":compare.output(["git", "-C", str(compare.HERE.parent), "rev-parse", "HEAD"]),
              "host":{"os":platform.system(), "arch":platform.machine()},
              "method":{"one_file_at_a_time":True, "rounds":args.rounds,
                        "order":"four-way rotating Latin square; both Rvdown modes balanced",
                        "profiles":PROFILES, "aria2_connection_cap":8,
                        "aria2_default_threshold":"20 MiB; all other comparison caps explicitly matched at 8",
                        "aria2_tuned_threshold":"1 MiB; same as earlier comparison",
                        "local_conditions":["4 MiB/s per active GET", "uncapped loopback"],
                        "timing":"process startup to file delivery, SHA256 outside timer",
                        "WAN":"shared loopback CONNECT byte relay, physical-interface-bound upstream, one DoH IP snapshot, TLS end-to-end, no external proxy",
                        "WAN_connection_evidence":"TCP connection count is observed, not a count of HTTP Range requests inside TLS",
                        "local_evidence":"server observes every HEAD/GET Range header and peak concurrent GETs",
                        "known_WAN_hash":"from previous independent successful tool outputs; no asymmetric warmup preflight"},
              "artifacts":{}, "samples":[], "runs":[], "summaries":{}}
    for name, binary in [("rvdown",args.rvdown),("aria2",args.aria2),("ffmpeg",args.ffmpeg)]:
        file = pathlib.Path(shutil.which(binary) or binary).resolve()
        report["artifacts"][name] = {"bytes":file.stat().st_size, "sha256":compare.digest(file)}
    report["harness_sha256"] = {name:compare.digest(compare.HERE/name)
                                for name in ["compare.py", "small_video.py"]}
    fixtures = ([generate(args, name, bitrate) for name, bitrate in
                 [("generated-sub2m",2000),("generated-4m",6000),("generated-8m",12000)]]
                if args.mode != "wan" else [])
    report["samples"].extend(meta for _,meta in fixtures)
    for file, meta in fixtures:
        for capped in [True, False]:
            label = meta["id"] + ("-capped" if capped else "-uncapped")
            with TraceFixture(file.read_bytes(), capped) as server:
                threading.Thread(target=server.serve_forever, daemon=True).start()
                series(args, report, label, f"http://127.0.0.1:{server.server_port}/bytes", meta["sha256"], server)
                server.shutdown()
    if args.mode != "local":
        host = urllib.parse.urlsplit(URL).hostname
        addresses = {host:compare.resolve_public(host, args)}
        with compare.DirectRelay(addresses, args.interface) as relay:
            threading.Thread(target=relay.serve_forever, daemon=True).start()
            args.relay_url = f"http://127.0.0.1:{relay.server_address[1]}"
            report["samples"].append({"id":"mdn-flower", "url":URL, "bytes":1128375, "sha256":KNOWN_SHA256})
            series(args, report, "mdn-flower", URL, KNOWN_SHA256, relay=relay)
            relay.shutdown()
    for sample in sorted({row["sample"] for row in report["runs"]}):
        report["summaries"][sample] = {profile:summary([row for row in report["runs"]
            if row["sample"]==sample and row["profile"]==profile], args.rounds) for profile in PROFILES}
    sample_count = {"all":7, "local":6, "wan":1}[args.mode]
    report["completion"] = {"complete":True, "planned_trials":sample_count*4*args.rounds,
                            "completed_trials":len(report["runs"])}
    compare.save(report, args)


if __name__ == "__main__":
    main()
