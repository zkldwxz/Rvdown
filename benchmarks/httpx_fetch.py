"""Independent httpx-fetch-style baseline: one GET, 1 MiB chunks, atomic output.

Network binding and a pre-resolved address map are benchmark controls only.
There is deliberately no segmented transfer, retry or resume implementation.
"""
import argparse
import json
import os
import socket
import sys
from pathlib import Path

import httpx


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("url")
    parser.add_argument("out")
    parser.add_argument("--interface")
    parser.add_argument("--dns-map")
    parser.add_argument("--proxy", help="benchmark-owned CONNECT relay; keeps TLS end-to-end")
    args = parser.parse_args()
    options = []
    if args.interface:
        index = socket.if_nametoindex(args.interface)
        if sys.platform == "darwin":
            options.append((socket.IPPROTO_IP, 25, index))  # IP_BOUND_IF
        elif sys.platform.startswith("linux"):
            options.append((socket.SOL_SOCKET, socket.SO_BINDTODEVICE,
                            args.interface.encode() + b"\0"))
        else:
            raise RuntimeError("interface binding unsupported on this platform")
    if args.dns_map:
        addresses = json.loads(Path(args.dns_map).read_text())
        original = socket.getaddrinfo

        def fixed_lookup(host, port, *rest, **kwargs):
            host = host.decode() if isinstance(host, bytes) else host
            if host not in addresses:
                raise OSError("unapproved DNS host")
            return original(addresses[host], port, socket.AF_INET, socket.SOCK_STREAM)

        socket.getaddrinfo = fixed_lookup
    dest = Path(args.out)
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = Path(str(dest) + ".part")
    transport = httpx.HTTPTransport(socket_options=options, trust_env=False, proxy=args.proxy)
    with httpx.Client(transport=transport, follow_redirects=True,
                      timeout=60.0, trust_env=False) as client:
        with client.stream("GET", args.url, headers={"User-Agent": "Rvdown-Benchmark/1.0",
                                                    "Accept-Encoding": "identity"}) as response:
            if response.status_code >= 400:
                print(json.dumps({"http_status": response.status_code}))
                return 2
            with part.open("wb") as output:
                for chunk in response.iter_bytes(1 << 20):
                    output.write(chunk)
    if not part.stat().st_size:
        return 1
    os.replace(part, dest)
    return 0


if __name__ == "__main__":
    sys.exit(main())
