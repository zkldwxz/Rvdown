# aria2 速度差、请求头与相关项目调研（2026-10-22）

## 实测结论与限制

**本报告属于 MDN 公共线路的辅助诊断，不是产品主基准。** 正式对照应按[视频优先计划](../../video-benchmark-plan.zh-CN.md)使用实际抖音 CDN，优先评价视频成功、完整交付、资源占用和接入体验；该三工具多样本对照尚未完成。

当前同场 MDN 视频测试是 aria2 较快，但**没有定位到“Rvdown 内核天生慢 2.3 倍”这样的确定根因**。旧一轮 10.14 对 4.39 秒只是当时样本；本轮五组各五次、全部完整通过，当前 CLI 的中位耗时 13.90 秒，aria2 同显式头 11.49 秒、默认头 10.34 秒，最快最慢范围明显重叠。不能把某轮倍率当成稳定的产品差距。

明确可见的结构差异是：Rvdown 先 HEAD 再 GET；aria2 从 GET 开始接收正文。但同一浏览器传输栈的直接 GET 在本轮也没有稳定更快，**不能把删除 HEAD 当成已经验证的提速修复**。生产默认没有更改。

## 五组公开单视频对照

源码提交：`551ed0cf2dbc7796b595c90739f3a51d7b7a4d29`，运行前源码干净。实际 CLI 与诊断示例二进制 SHA256、调度脚本 SHA256 都在[逐条结果](speed-causes-results.json)中。

- 同一 MDN 公开视频、1,128,375 bytes，固定 SHA256；一个文件一个连接，无预热下载，五轮循环轮换位置。每次完整长度和 SHA256 均需通过；失败不会被移出计划分母。
- 所有组走同一个基准自有本机 CONNECT 字节转发器，上游显式绑定物理接口并使用同一公开 DoH IP 快照。TLS 端到端，没有外部代理或 TLS 替换。共同转发器有开销，这不是无转发器极限吞吐排行。
- 复用旧对照的显式 `User-Agent: Rvdown-Benchmark/1.0` 和 `Accept-Encoding: identity`，**所有公网组均无显式 Referer**。aria2 默认头组只移除调用者的 UA/编码头，其它传输设置不变；两组 aria2 都禁用配置文件/netrc，单连接、一次尝试。
- “统一显式头”不等于整个线上 HTTP 请求完全一致：浏览器仿真栈和 aria2 仍有各自内建头、TLS 与协议设置。原版 aria2 的 [HTTP 请求实现](https://github.com/aria2/aria2/blob/master/src/HttpRequest.cc)使用 HTTP/1.1；两个原生诊断明确强制并记录 HTTP2。没有将协议差异冒充只改了语言。
- 主表使用外部进程总时间，含启动与最终写文件，完整 SHA256 在计时外。两个 wreq 诊断使用相同 Chrome131 配置，小文件先收进内存再落盘，不能当作完整 SDK/CLI 交付路径；其内部 transport_elapsed_s 单列且不含最后写盘。

| 组别 | 完整成功/计划 | 中位耗时 s | 最快–最慢 s |
|---|---:|---:|---:|
| Rvdown 当前 CLI | 5/5 | 13.897 | 5.283–42.639 |
| 同传输栈诊断：HEAD→GET | 5/5 | 8.242 | 5.788–13.888 |
| 同传输栈诊断：直接 GET | 5/5 | 12.158 | 4.782–18.839 |
| aria2：统一显式 UA/编码头 | 5/5 | 11.488 | 5.487–13.134 |
| aria2：不传额外请求头 | 5/5 | 10.338 | 6.452–17.859 |

全部 25 次只有一个上游 TCP 连接、完整摘要相同。当前 CLI 五次都报告 attempts=2、network_bytes=1,128,375、resumed_bytes=0；没有反复重下或多传一个文件的证据。[CLI 数值核对](speed-causes-cli-audit.json)

## 已确认与未确认的原因

1. **多一个探测往返，有成本，但不足以单独归因。** 原生 HEAD 响应时间 0.969–2.896 秒、中位 1.283 秒；该值**包含第一次 TCP/TLS 握手**，不能全当作删掉 HEAD 能节省的时间。HEAD 完成到 GET 首段正文还有约 0.842–1.230 秒。直接 GET 诊断中位 12.158 秒，比 HEAD→GET 的 8.242 秒更长，说明本轮线路/服务侧波动足以盖过一个往返；这不是 HEAD 会加速的因果证据。
2. **主要等待发生在网络阶段，CPU 计算没有跑满。** 原生诊断从 GET 首字节到正文收完为约 1.98–16.13 秒。CLI 进程 CPU user+system 的五次中位合计约 0.03 秒，而总耗时中位 13.90 秒；不能解释为 Rust 算不过来。这个指标不包含共享 Python 转发器的 CPU，不能拿来做整机 CPU 胜出结论。
3. **不是这个小文件没开够多线程，也不是额外 ffmpeg/MD5。** 该来源低于 Rvdown 的分段门槛，本轮统一单连接，旧小视频“8”也只是上限；对照已关闭额外媒体检验和 MD5。此前 16 MiB 多连接回环两者约 24 MiB/s，说明本地实现具备接近的持续吞吐；仍不能替代公网大文件证据。
4. **TLS/HTTP2、内建头、CDN 对请求的处理和 TCP 接收路径仍是待隔离因素。** [上一轮重连/协议测试](reconnect-investigation.zh-CN.md)没有显示强制 HTTP1.1 或换浏览器必然更快。现有一个公网来源/一个出口，尚不足以定出其中哪项贡献多少；不能据此宣称反爬强度不足，也不能把所有慢传归为本机代理。

当前最值得验证的实现方向是：首个 GET 同时取得元信息并保存正文、避免无条件串行预检；动态分配剩余区间和处理慢尾段；按来源测量真实 TCP 并发及协议表现。它们都需要保住强验证器、Range/资源身份校验和断点语义，不是本次已交付的生产优化。

## 不传 Referer/自定义 headers，aria2 能否下载

**能否下载由目标站点决定，不是 aria2 必须带 Referer 才工作。** 本轮不传额外 UA/编码/Referer 的 aria2 **5/5** 完整下载 MDN，摘要与其它组相同。默认 aria2 仍自动发送 `User-Agent: aria2/1.37.0`、Host、Accept、Want-Digest 等标准头；“没有传自定义头”不等于“HTTP 没有头”。[aria2 官方头/Referer 参数](https://aria2.github.io/manual/en/html/aria2c.html#cmdoption-referer)

另有本地构造的 Referer 门禁夹具，只验证机制，不是真实 CDN 验收：

| 场景 | 额外 Referer | 实际结果 |
|---|---|---|
| 开放夹具 | 无 | 200，完整 32 KiB |
| 校验 Referer 的夹具 | 无 | 403，实际下载失败 |
| 同一门禁夹具 | 正确值 | 200，完整 32 KiB |

这三个是预期行为全部吻合，其中 403 **仍然是一次下载失败**，没有算成公网成功。真实平台可能同时检查 Referer、UA、Cookie、URL 时效和 TLS；补 HTTP 请求头也不等于更换 TLS 指纹。私有业务 CDN 样本、签名 URL 和请求头不进入这份公开证据。

## GitHub 项目调研：内核、重写和封装要分清

以下为 2026-10-22 读取的一手仓库文档/源码。没有运行这些第三方项目，所以**作者成绩不计入我们的实测**。

| 项目 | 实现/与 aria2 的关系 | 性能上值得看的内容 | 证据边界 |
|---|---|---|---|
| [Aria2 Pro Core](https://github.com/P3TERX/Aria2-Pro-Core) | C++ aria2 加补丁，Linux 静态构建 | 放宽连接数与最小分段；低速/断连重试 | README 给出补丁清单，没有证明所有来源都更快；线程越多不等于越快 |
| [aria2-build-msys2](https://github.com/myfreeer/aria2-build-msys2) | Windows C++ 补丁构建 | 默认 split=128、每服务器连接=16、分段门槛1MiB，低速重试 | 是参数/补丁增强，不能把默认参数变化当内核效率收益 |
| [Aria2 Next](https://github.com/AnInsomniacy/aria2-next) | C++20/原 aria2 接口，HTTP 改用 libcurl multi | 动态分段、慢尾请求竞速、按主机处理429/503、独立磁盘池 | 有故障/吞吐脚本；未在本机同条件运行，不能给速度冠军 |
| [aria2_rust](https://github.com/balovess/aria2_rust) | 真正 Rust 引擎重写，非 RPC 皮肤 | bytes 数据路径、后台磁盘/hash、事件驱动等待 | 作者明确标注未生产就绪；尚无与 C++ 原版同负载完整下载基线 |
| [Hydra](https://github.com/ja7ad/hydra) | 独立 Rust 下载引擎，**不是 aria2 fork** | 依实测速率动态抢段、停滞回收、并发/镜像调度，提供库 | 作者发布了可复现对比，但未在我们的实际 CDN 验证 |
| [aria2-rs](https://github.com/ihciah/aria2-rs) / [Motrix](https://github.com/agalwood/Motrix) / [Rayburst](https://github.com/AnInsomniacy/rayburst) | 分别是 Rust RPC 客户端、Electron 客户端、Rust/Tauri 客户端（引擎为 Aria2 Next） | 应用接入和任务管理 | 外层使用 Rust 不表示下载内核被 Rust 重写，也不构成传输提速证据 |

**性能策略优先看 Aria2 Next 和 Hydra；Rust 重写可跟踪 aria2_rust，但先核对成熟度。** [Aria2 Next 源码](https://github.com/AnInsomniacy/aria2-next/blob/main/src/http/SegmentPlanner.cc)确实按速率和连接启动成本分配区间，并对慢尾竞速；其[架构说明](https://github.com/AnInsomniacy/aria2-next/blob/main/docs/architecture.md)还区分并行 Range 的 HTTP1.1 与单流 HTTP2。这些比单纯解除线程上限更值得研究。

[Hydra 调度源码](https://github.com/ja7ad/hydra/blob/main/crates/hydra-core/src/sched.rs)采用平滑速率、观测窗口及持续偏离判定后再迁移剩余工作，并显式处理缩段通知，避免重叠下载制造更多拥塞。其 [README 基准](https://github.com/ja7ad/hydra#benchmark)声明在 100ms 人工链路的 1GB/8连接测试中约 414 对 aria2 的 298 MB/s；是作者环境中的平均值，不能转写成我们会快 39%，也不是其所有公网行都领先。

[aria2_rust 自己的对比](https://github.com/balovess/aria2_rust/blob/master/README_CN.md#性能)列出 Windows 空闲 RPC 内存/体积，Rust 版也并未全面低于原版；算法微基准不等于完整 HTTP 下载更快。README 链接的一些性能/迁移明细文件在本次 API 读取中返回404，未拿这些缺失文件当作已验证证据。

## 复现和检查

```sh
cargo test --locked -p rvdown --example request_probe
cargo build --locked --release -p rvdown-cli
cargo build --locked --release -p rvdown --example request_probe
python3 benchmarks/speed_causes.py --rvdown target/release/rvdown \
  --probe target/release/examples/request_probe --aria2 aria2c \
  --interface YOUR_PHYSICAL_INTERFACE --rounds 5 \
  --work /tmp/rvdown-speed-causes-new --report /tmp/rvdown-speed-causes.json
```

源码只增加诊断示例/调度及离线测试，不改生产内核默认；默认 Rust 175 项、请求诊断示例单独2项、Python17项通过，clippy/fmt通过。检查范围和样本站/透明路由限制沿用[前一轮说明](reconnect-investigation.zh-CN.md#样本来源和代理环境的边界)。成功媒体自动清理；只提交筛选后的 JSON，不上传原始日志、输入配置、DNS 映射或本机状态。
