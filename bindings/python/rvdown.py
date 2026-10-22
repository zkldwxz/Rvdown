"""Rvdown 的 Python 薄封装：spawn 可执行文件，解析 JSON Lines。

不做任何下载逻辑，字段与 [调用文档](../../docs/calling.zh-CN.md) 完全一致。
升级只需替换二进制（或传新的路径）。
"""

from __future__ import annotations

import json
import subprocess
import threading
from pathlib import Path
from typing import IO, Iterable, Iterator


class RvdownError(RuntimeError):
    """可执行文件缺失或启动失败（任务级失败不抛异常，走结果里的 code）。"""


class Rvdown:
    def __init__(self, binary: str = "rvdown", *, ffmpeg: str | None = None) -> None:
        self.binary = binary
        self.ffmpeg = ffmpeg

    def _common_args(self) -> list[str]:
        # 通用选项属于子命令（clap 不接受放在子命令前），所以一律追加在子命令之后。
        return ["--ffmpeg", self.ffmpeg] if self.ffmpeg else []

    def get(self, url: str, out: str | Path, **options) -> dict:
        """单文件下载，返回结果字典（包含 code）。"""
        args = [self.binary, "get", url, "--out", str(out), "--json"]
        args += self._common_args() + _flags(options)
        completed = subprocess.run(args, capture_output=True, text=True, check=False)
        if not completed.stdout.strip():
            raise RvdownError(completed.stderr.strip() or "rvdown produced no output")
        return json.loads(completed.stdout.strip().splitlines()[-1])

    def run(
        self,
        jobs: Iterable[dict],
        *,
        progress: bool = False,
        concurrency: int | None = None,
        **options,
    ) -> Iterator[dict]:
        """批量下载：逐条产出结果字典（边下边读）。"""
        args = [self.binary, "run", "--tasks", "-"] + self._common_args()
        if progress:
            args.append("--progress")
        if concurrency:
            args += ["--concurrency", str(concurrency)]
        args += _flags(options)
        payload = "\n".join(json.dumps(job, ensure_ascii=False) for job in jobs) + "\n"
        process = subprocess.Popen(
            args,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        assert process.stdin is not None and process.stdout is not None
        # stdin 与 stderr 各开一个线程：任务很多时写 stdin、或 INFO 日志写满 stderr 管道，
        # 都会与这里读 stdout 互相等待而死锁。
        stderr_chunks: list[str] = []
        writer = threading.Thread(target=_feed, args=(process.stdin, payload), daemon=True)
        drainer = threading.Thread(
            target=_drain, args=(process.stderr, stderr_chunks), daemon=True
        )
        writer.start()
        drainer.start()
        try:
            for line in process.stdout:
                line = line.strip()
                if not line:
                    continue
                event = json.loads(line)
                if event.get("event") == "result":
                    yield event
        finally:
            process.stdout.close()
            code = process.wait()
            writer.join()
            drainer.join()
            if code not in (0, 1, 130):
                stderr = "".join(stderr_chunks)
                raise RvdownError(f"rvdown exited with {code}: {stderr.strip()}")


def _feed(stream: IO[str], payload: str) -> None:
    try:
        stream.write(payload)
        stream.close()
    except (BrokenPipeError, OSError):
        # 进程提前退出（例如参数错误）时由退出码报告，这里不重复抛错。
        pass


def _drain(stream: IO[str] | None, chunks: list[str]) -> None:
    if stream is None:
        return
    for chunk in stream:
        chunks.append(chunk)


def _flags(options: dict) -> list[str]:
    """把常用选项翻译成命令行参数；未知键直接报错，避免静默忽略。"""
    mapping = {
        "connections": "--connections",
        "retry_attempts": "--retry-attempts",
        "proxy": "--proxy",
        "interface": "--interface",
        "min_output_bytes": "--min-output-bytes",
        "dns_doh": "--dns-doh",
        "dns_doh_ip": "--dns-doh-ip",
    }
    flags: list[str] = []
    for key, value in options.items():
        if key == "no_proxy" and value:
            flags.append("--no-proxy")
        elif key == "no_verify" and value:
            flags.append("--no-verify")
        elif key == "no_checksum" and value:
            flags.append("--no-checksum")
        elif key == "headers" and value:
            for name, header_value in value.items():
                flags += ["--header", f"{name}: {header_value}"]
        elif key in mapping:
            flags += [mapping[key], str(value)]
        else:
            raise ValueError(f"unknown option: {key}")
    return flags


__all__ = ["Rvdown", "RvdownError"]
