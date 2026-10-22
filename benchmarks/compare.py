#!/usr/bin/env python3
"""Reproducible, public-data-only video downloader comparison.

Raw process logs and media stay in --work (never commit that directory).
The explicitly constructed report contains no environment dump or local paths.
"""
import argparse
import contextlib
import hashlib
import http.server
import ipaddress
import json
import os
import pathlib
import platform
import re
import select
import shutil
import signal
import socket
import socketserver
import struct
import statistics
import subprocess
import sys
import threading
import time
import urllib.parse

HERE = pathlib.Path(__file__).resolve().parent
MIB = 1024 * 1024
TOOLS = ["rvdown-8", "httpx-fetch", "aria2-8"]
SAMPLES = [
    ("mdn-flower", "https://interactive-examples.mdn.mozilla.net/media/cc0-videos/flower.mp4"),
    ("ffmpeg-4m", "https://samples.ffmpeg.org/MPEG-4/mp4video_mp3audio.mp4"),
]


def public_url(value):
    url = urllib.parse.urlsplit(value)
    if (url.scheme != "https" or not url.hostname or url.username or url.password
            or url.query or url.fragment or url.port not in (None, 443)):
        raise ValueError("public sample must be credential-free HTTPS without query/fragment")
    try:
        address = ipaddress.ip_address(url.hostname)
    except ValueError:
        if "." not in url.hostname or url.hostname.endswith((".local", ".internal", ".test")):
            raise ValueError("non-public host")
    else:
        if not address.is_global:
            raise ValueError("non-public address")
    return value


def valid_result(code, size, actual_hash, expected_hash):
    return code == 0 and size > 0 and actual_hash == expected_hash


def round_order(tools, number):
    offset = number % len(tools)
    return tools[offset:] + tools[:offset]


def summarize(rows):
    successes = [row for row in rows if row["ok"]]
    rates = [row["bytes"] / MIB / row["elapsed_s"] for row in successes]
    return {
        "attempted": len(rows), "passed": len(successes),
        "failed": len(rows) - len(successes),
        "median_mib_s_successes_only": round(statistics.median(rates), 3) if rates else None,
        "min_mib_s_successes_only": round(min(rates), 3) if rates else None,
        "max_mib_s_successes_only": round(max(rates), 3) if rates else None,
        "median_peak_rss_bytes": statistics.median(
            [row["peak_rss_bytes"] for row in successes if row.get("peak_rss_bytes")]
        ) if any(row.get("peak_rss_bytes") for row in successes) else None,
    }


def digest(file):
    result = hashlib.sha256()
    with pathlib.Path(file).open("rb") as stream:
        for block in iter(lambda: stream.read(MIB), b""):
            result.update(block)
    return result.hexdigest()


def clean_environment():
    # Explicitly select essential process variables. Do not inherit tokens or business config.
    allowed = ("PATH", "TMPDIR", "SYSTEMROOT", "SSL_CERT_FILE", "SSL_CERT_DIR")
    env = {key: os.environ[key] for key in allowed if key in os.environ}
    env.update(NO_PROXY="*", no_proxy="*", PYTHONIOENCODING="utf-8")
    return env


def output(command):
    return subprocess.check_output(command, text=True, env=clean_environment(),
                                   stderr=subprocess.DEVNULL).strip()


def resolve_public(host, args):
    command = ["curl", "--noproxy", "*", "--max-time", "20", "-fsS",
               "--resolve", f"{urllib.parse.urlsplit(args.doh).hostname}:443:{args.doh_ip}"]
    if args.interface:
        command += ["--interface", args.interface]
    command += [args.doh + "?" + urllib.parse.urlencode({"name": host, "type": "A"})]
    answer = json.loads(output(command))
    values = [item["data"] for item in answer.get("Answer", []) if item.get("type") == 1]
    if not values or any(not ipaddress.ip_address(value).is_global for value in values):
        raise ValueError("DNS returned no public IPv4 address")
    return values[0]


def relay_target(authority, addresses):
    host, port = authority.rsplit(":", 1)
    address = addresses.get(host.lower())
    if port != "443" or not address or not ipaddress.ip_address(address).is_global:
        raise ValueError("unapproved relay destination")
    return address


class DirectRelay(socketserver.ThreadingTCPServer):
    """CONNECT byte relay; TLS stays end-to-end. No OS proxy/DNS configuration changes."""
    daemon_threads = True
    request_queue_size = 128

    def __init__(self, addresses, interface):
        self.addresses = addresses
        self.interface = interface
        self.connections = 0
        self.lock = threading.Lock()
        super().__init__(("127.0.0.1", 0), RelayHandler)

    def handle_error(self, *_):
        pass


class RelayHandler(socketserver.BaseRequestHandler):
    def handle(self):
        request = b""
        self.request.settimeout(15)
        try:
            while b"\r\n\r\n" not in request:
                data = self.request.recv(4096)
                if not data or len(request) > 16384:
                    return
                request += data
            line = request.split(b"\r\n", 1)[0].decode("ascii").split()
            if len(line) != 3 or line[0] != "CONNECT":
                self.request.sendall(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
                return
            address = relay_target(line[1], self.server.addresses)
        except (ValueError, UnicodeError, OSError):
            self.request.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            return
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as remote:
            if self.server.interface:
                if sys.platform == "darwin":
                    remote.setsockopt(socket.IPPROTO_IP, 25, socket.if_nametoindex(self.server.interface))
                elif sys.platform.startswith("linux"):
                    remote.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, self.server.interface.encode() + b"\0")
                else:
                    raise RuntimeError("physical binding unsupported")
            remote.settimeout(15)
            try:
                remote.connect((address, 443))
            except OSError:
                self.request.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                return
            with self.server.lock:
                self.server.connections += 1
            self.request.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            remote.settimeout(30)
            self.request.settimeout(30)
            try:
                while True:
                    readable, _, _ = select.select([self.request, remote], [], [], 35)
                    if not readable:
                        return
                    for source in readable:
                        data = source.recv(65536)
                        if not data:
                            return
                        (remote if source is self.request else self.request).sendall(data)
            except OSError:
                return


def command_for(tool, url, dest, args, local=False):
    connections = "1" if tool == "rvdown-1" else "8"
    if tool.startswith("rvdown"):
        command = [args.rvdown, "get", url, "--out", str(dest), "--json",
                   "--connections", connections, "--retry-attempts", "1", "--no-proxy",
                   "--no-verify", "--no-checksum", "--header", "User-Agent: Rvdown-Benchmark/1.0",
                   "--header", "Accept-Encoding: identity"]
        if not local and args.relay_url:
            command.remove("--no-proxy")
            command += ["--proxy", args.relay_url]
        elif not local:
            command += ["--dns-doh", args.doh, "--dns-doh-ip", args.doh_ip]
            if args.interface:
                command += ["--interface", args.interface]
        return command
    if tool == "httpx-fetch":
        command = [args.python, str(HERE / "httpx_fetch.py"), url, str(dest)]
        if not local and args.relay_url:
            command += ["--proxy", args.relay_url]
        elif not local:
            command += ["--dns-map", str(args.work / "dns.json")]
            if args.interface:
                command += ["--interface", args.interface]
        return command
    command = [args.aria2, "--no-conf=true", "--all-proxy=", "--http-proxy=", "--https-proxy=",
               "--disable-ipv6=true", "--max-connection-per-server=8", "--split=8",
               "--min-split-size=1M", "--file-allocation=none", "--max-tries=1",
               "--connect-timeout=15", "--timeout=30", "--continue=true",
               "--auto-file-renaming=false", "--allow-overwrite=true", "--summary-interval=0",
               "--console-log-level=warn", "--download-result=hide", "--auto-save-interval=1",
               "--user-agent=Rvdown-Benchmark/1.0", "--header=Accept-Encoding: identity",
               "--dir=" + str(dest.parent), "--out=" + dest.name, url]
    if not local:
        if args.relay_url:
            command += ["--all-proxy=" + args.relay_url, "--http-proxy=" + args.relay_url,
                        "--https-proxy=" + args.relay_url, "--no-proxy="]
        elif args.interface:
            command += ["--interface=" + args.interface]
    return command


def measure(command, args, label, interrupt_after=None):
    stdout = args.work / (label + ".stdout")
    stderr = args.work / (label + ".stderr")
    if sys.platform == "darwin":
        command = ["/usr/bin/time", "-l"] + command
    started = time.perf_counter()
    timed_out = False
    with stdout.open("w") as out, stderr.open("w") as err:
        process = subprocess.Popen(command, stdout=out, stderr=err, env=clean_environment(),
                                   start_new_session=True)
        try:
            process.wait(timeout=interrupt_after or args.timeout)
        except subprocess.TimeoutExpired:
            timed_out = interrupt_after is None
            os.killpg(process.pid, signal.SIGINT if interrupt_after else signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
    elapsed = time.perf_counter() - started
    text = stderr.read_text(errors="replace")
    rss = re.search(r"(\d+)\s+maximum resident set size", text)
    cpu = re.search(r"([\d.]+)\s+real\s+([\d.]+)\s+user\s+([\d.]+)\s+sys", text)
    raw = stdout.read_text(errors="replace")
    events = []
    for line in raw.splitlines():
        with contextlib.suppress(ValueError):
            events.append(json.loads(line))
    status = None
    match = re.search(r"(?:HTTP status|status[=: ]|HTTP response status code[=: ]|HTTP )(\d{3})", raw + text)
    if match:
        status = int(match[1])
    return {
        "exit_code": process.returncode, "timed_out": timed_out,
        "elapsed_s": round(elapsed, 6),
        "peak_rss_bytes": int(rss[1]) if rss else None,
        "cpu_user_s": float(cpu[2]) if cpu else None,
        "cpu_system_s": float(cpu[3]) if cpu else None,
        "http_status": status,
    }, events


class RangeFixture(http.server.ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 128

    def __init__(self):
        super().__init__(("127.0.0.1", 0), FixtureHandler)
        self.data = bytes(range(256)) * (16 * MIB // 256)
        self.lock = threading.Lock()
        self.bytes_sent = self.requests = self.active = self.max_active = 0
        self.ranges = 0
        self.throttled = True
        self.resume = False
        self.media = None

    def reset(self):
        with self.lock:
            self.bytes_sent = self.requests = self.ranges = self.max_active = 0

    def handle_error(self, *_):
        # Probe/abort tests intentionally reset sockets; keep them out of the measurement log.
        pass

    def stats(self):
        with self.lock:
            return {"application_bytes_served": self.bytes_sent, "requests": self.requests,
                    "range_requests": self.ranges, "peak_concurrent_requests": self.max_active}


class FixtureHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_HEAD(self):
        self.serve(False)

    def do_GET(self):
        self.serve(True)

    def serve(self, body):
        server = self.server
        data = server.data
        if self.path != "/bytes":
            path = pathlib.PurePosixPath(urllib.parse.urlsplit(self.path).path)
            if not server.media or ".." in path.parts:
                self.send_error(404)
                return
            file = server.media.joinpath(*path.parts[1:])
            if not file.is_file():
                self.send_error(404)
                return
            data = file.read_bytes()
        total = len(data)
        start, end = 0, total - 1
        value = self.headers.get("Range")
        if value:
            match = re.fullmatch(r"bytes=(\d+)-(\d*)", value)
            if not match:
                self.send_error(416)
                return
            start = int(match[1])
            end = min(int(match[2]) if match[2] else end, end)
        if start > end:
            self.send_error(416)
            return
        self.send_response(206 if value else 200)
        self.send_header("Content-Length", str(end - start + 1))
        self.send_header("Content-Type", "application/vnd.apple.mpegurl" if self.path.endswith(".m3u8") else "application/octet-stream")
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("ETag", '"public-deterministic-fixture-v1"')
        if value:
            self.send_header("Content-Range", f"bytes {start}-{end}/{total}")
        self.end_headers()
        if not body:
            return
        with server.lock:
            server.requests += 1
            server.ranges += bool(value)
            server.active += 1
            server.max_active = max(server.max_active, server.active)
        rate = 4 * MIB
        if server.resume and start >= total - 2 * MIB:
            rate = MIB // 4
        try:
            for offset in range(start, end + 1, 65536):
                chunk = data[offset:min(offset + 65536, end + 1)]
                self.wfile.write(chunk)
                self.wfile.flush()
                with server.lock:
                    server.bytes_sent += len(chunk)
                if server.throttled:
                    time.sleep(len(chunk) / rate)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with server.lock:
                server.active -= 1


def save(report, args):
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")


def benchmark(args):
    args.work.mkdir(parents=True, exist_ok=True)
    revision = output(["git", "-C", str(HERE.parent), "rev-parse", "HEAD"])
    report = {"schema": 1, "date": time.strftime("%Y-%m-%d", time.gmtime()),
              "rvdown_commit": revision, "host": {"os": platform.system(),
              "os_release": platform.release(), "arch": platform.machine()},
              "method": {"repetitions": args.rounds, "order": "rotating Latin square",
              "timing": "process startup through atomic file delivery; post-run SHA256 excluded",
              "rvdown_checks_in_transfer_comparison": "ffmpeg and MD5 disabled; all tools SHA256 verified afterward",
              "network": "explicit physical interface, proxies disabled, public DNS" if args.interface else "system interface; proxies disabled",
              "dns": "Rvdown/httpx: DoH; aria2: same public provider via UDP; CDN address may differ",
              "local_fixture": "16 MiB deterministic bytes; 4 MiB/s per connection; not an internet speed result"},
              "artifacts": {}, "runs": [], "resume": [], "summaries": {}}
    for name, binary in [("rvdown", args.rvdown), ("aria2", args.aria2), ("python", args.python)]:
        file = pathlib.Path(shutil.which(binary) or binary).resolve()
        report["artifacts"][name] = {"executable_bytes": file.stat().st_size,
                                       "sha256": digest(file)}
    report["versions"] = {"rvdown": output([args.rvdown, "--version"]),
                           "aria2": output([args.aria2, "--version"]).splitlines()[0],
                           "httpx": output([args.python, "-c", "import httpx; print(httpx.__version__)"]),
                           "python": output([args.python, "--version"])}
    samples = (json.loads(args.samples.read_text()) if args.samples else SAMPLES) if args.mode in ("all", "wan") else []
    for _, url in samples:
        public_url(url)
    dns = {urllib.parse.urlsplit(url).hostname: resolve_public(urllib.parse.urlsplit(url).hostname, args)
           for _, url in samples}
    (args.work / "dns.json").write_text(json.dumps(dns))
    relay = DirectRelay(dns, args.interface)
    relay_thread = threading.Thread(target=relay.serve_forever, daemon=True)
    relay_thread.start()
    args.relay_url = f"http://127.0.0.1:{relay.server_address[1]}"
    report["method"]["dns"] = "one public DoH snapshot; same fixed IPv4 target for all tools; no system DNS fallback"
    report["method"]["network"] = "all tools use one benchmark-owned loopback CONNECT byte relay; outbound sockets bind the physical interface; end-to-end TLS; no external proxy or system changes"
    report["harness_sha256"] = {name: digest(HERE / name) for name in ["compare.py", "httpx_fetch.py"]}
    for sample, url in samples:
        public_url(url)
        reference = args.work / (sample + "-reference.mp4")
        # Rvdown one-time preflight. Failure stays in the report and every timed trial still runs.
        preflight, _ = measure(command_for("rvdown-8", url, reference, args), args, sample + "-preflight")
        expected = digest(reference) if preflight["exit_code"] == 0 and reference.exists() else None
        report.setdefault("public_samples", []).append({"id": sample, "url": url,
            "sha256": expected, "bytes": reference.stat().st_size if expected else None,
            "preflight_exit_code": preflight["exit_code"]})
        run_series(sample, url, expected, args, report, False)
        save(report, args)
    if args.mode in ("all", "local"):
        with RangeFixture() as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            url = f"http://127.0.0.1:{server.server_port}/bytes"
            expected = hashlib.sha256(server.data).hexdigest()
            run_series("local-capped", url, expected, args, report, True, server)
            server.throttled = False
            run_series("local-uncapped", url, expected, args, report, True, server)
            server.throttled = server.resume = True
            for tool in TOOLS:
                dest = args.work / ("resume-" + tool + ".bin")
                command = command_for(tool, url, dest, args, True)
                first, _ = measure(command, args, "resume-" + tool + "-interrupted", 1.2)
                time.sleep(0.4)
                server.reset()
                second, events = measure(command, args, "resume-" + tool + "-continued")
                actual = digest(dest) if dest.exists() else None
                stats = server.stats()
                result = {"tool": tool, "interrupt_exit_code": first["exit_code"],
                          "restart": second, "sha256_verified": actual == expected,
                          "second_request_bytes_less_than_full": stats["application_bytes_served"] < len(server.data),
                          **stats}
                if events:
                    result["rvdown_resumed_bytes"] = events[-1].get("resumed_bytes", 0)
                report["resume"].append(result)
                print(json.dumps({"resume": tool, **result}), flush=True)
                save(report, args)
            server.shutdown()
    for sample in sorted({row["sample"] for row in report["runs"]}):
        report["summaries"][sample] = {tool: summarize([row for row in report["runs"]
             if row["sample"] == sample and row["tool"] == tool]) for tool in TOOLS + ["rvdown-1"]}
    report["media_in_public_report"] = False
    report["physical_relay_connections"] = relay.connections
    relay.shutdown()
    relay.server_close()
    save(report, args)


def run_series(sample, url, expected, args, report, local, server=None):
    # Three-way order is balanced over three rounds. Single-connection sensitivity runs afterward.
    for number in range(args.rounds):
        for tool in round_order(TOOLS, number):
            one_run(sample, url, expected, tool, number, args, report, local, server)
    for number in range(args.rounds):
        one_run(sample, url, expected, "rvdown-1", number, args, report, local, server)


def one_run(sample, url, expected, tool, number, args, report, local, server):
    label = f"{sample}-{number}-{tool}"
    dest = args.work / (label + ".bin")
    if server:
        server.reset()
    measured, _ = measure(command_for(tool, url, dest, args, local), args, label)
    size = dest.stat().st_size if dest.exists() else 0
    actual = digest(dest) if size else None
    row = {"sample": sample, "round": number + 1, "tool": tool, **measured,
           "bytes": size, "sha256": actual,
           "ok": expected is not None and valid_result(measured["exit_code"], size, actual, expected)}
    if server:
        row["server"] = server.stats()
    report["runs"].append(row)
    print(json.dumps(row), flush=True)
    save(report, args)
    if row["ok"]:
        dest.unlink()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rvdown", default="rvdown")
    parser.add_argument("--aria2", default="aria2c")
    parser.add_argument("--python", default=sys.executable)
    parser.add_argument("--work", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--interface")
    parser.add_argument("--doh", default="https://dns.alidns.com/resolve")
    parser.add_argument("--doh-ip", default="223.5.5.5")
    parser.add_argument("--dns-server", default="223.5.5.5")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--mode", choices=("all", "wan", "local"), default="all")
    parser.add_argument("--samples", type=pathlib.Path, help="public [id, HTTPS URL] pairs; no query strings")
    args = parser.parse_args()
    if args.interface and (not re.fullmatch(r"[A-Za-z0-9_-]+", args.interface)
                           or args.interface.startswith(("tun", "utun", "tap", "lo"))):
        parser.error("a physical interface is required")
    if args.rounds < 3:
        parser.error("at least three rounds are required")
    if any(args.work.iterdir()) if args.work.exists() else False:
        parser.error("--work must be a new, empty scratch directory")
    benchmark(args)


if __name__ == "__main__":
    main()
