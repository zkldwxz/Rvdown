// Rvdown 的 Node 薄封装：spawn 可执行文件，解析 JSON Lines。
// 不做任何下载逻辑，字段与 docs/calling.zh-CN.md 完全一致。

import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

export class RvdownError extends Error {}

export class Rvdown {
  constructor(binary = "rvdown", { ffmpeg } = {}) {
    this.binary = binary;
    this.ffmpeg = ffmpeg;
  }

  // 通用选项属于子命令（clap 不接受放在子命令前），所以一律追加在子命令之后。
  #commonArgs() {
    return this.ffmpeg ? ["--ffmpeg", this.ffmpeg] : [];
  }

  /** 单文件下载，解析为结果对象（含 code）。 */
  async get(url, out, options = {}) {
    const args = ["get", url, "--out", out, "--json", ...this.#commonArgs(), ...flags(options)];
    const { stdout, stderr, code } = await collect(this.binary, args);
    const line = stdout.trim().split("\n").pop();
    if (!line) throw new RvdownError(stderr.trim() || `rvdown exited with ${code}`);
    return JSON.parse(line);
  }

  /** 批量下载：异步迭代结果对象（边下边读）。 */
  async *run(jobs, { progress = false, concurrency, ...options } = {}) {
    const args = ["run", "--tasks", "-", ...this.#commonArgs(), ...flags(options)];
    if (progress) args.push("--progress");
    if (concurrency) args.push("--concurrency", String(concurrency));

    const child = spawn(this.binary, args, { stdio: ["pipe", "pipe", "pipe"] });
    let stderr = "";
    child.stderr.on("data", (chunk) => {
      stderr += chunk.toString();
    });
    child.stdin.end(jobs.map((job) => JSON.stringify(job)).join("\n") + "\n");

    const lines = createInterface({ input: child.stdout });
    for await (const line of lines) {
      if (!line.trim()) continue;
      const event = JSON.parse(line);
      if (event.event === "result") yield event;
    }
    const code = await new Promise((resolve) => child.on("close", resolve));
    if (code !== 0 && code !== 1 && code !== 130) {
      throw new RvdownError(`rvdown exited with ${code}: ${stderr.trim()}`);
    }
  }
}

function collect(command, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => {
      stdout += chunk.toString();
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk.toString();
    });
    child.on("error", reject);
    child.on("close", (code) => resolve({ stdout, stderr, code }));
  });
}

function flags(options) {
  const mapping = {
    connections: "--connections",
    retry_attempts: "--retry-attempts",
    proxy: "--proxy",
    interface: "--interface",
    min_output_bytes: "--min-output-bytes",
    dns_doh: "--dns-doh",
    dns_doh_ip: "--dns-doh-ip",
  };
  const out = [];
  for (const [key, value] of Object.entries(options)) {
    if (key === "no_proxy" && value) out.push("--no-proxy");
    else if (key === "no_verify" && value) out.push("--no-verify");
    else if (key === "no_checksum" && value) out.push("--no-checksum");
    else if (key === "headers" && value) {
      for (const [name, headerValue] of Object.entries(value)) out.push("--header", `${name}: ${headerValue}`);
    } else if (key in mapping) out.push(mapping[key], String(value));
    else throw new Error(`unknown option: ${key}`);
  }
  return out;
}
