# Pingora 0.9.1 HTTP 数据面取证结论

> 一次性资产的结论页。所有判断来自本机实测（L2）或锁定的 0.9.1 源码（L0），没有任何一条来自
> 文档记忆。`./scripts/run-probes.sh` 可完整重放；原始日志在 `.evidence/`（不提交）。
>
> 源码路径统一写作 `~/.cargo/registry/src/index.crates.io-*/pingora-core-0.9.1/...`，
> 实际目录可用 `ls -d ~/.cargo/registry/src/*/pingora-core-0.9.1` 定位，版本必须仍是 0.9.1。
>
> 文中端口、块计数、毫秒数取自某一次具体运行；复现时这些数值会变，判定结论不变。

## 1. 结论摘要

| 能力 | 结论 | 证据等级 | 证据 | 对 nico-http 的影响 |
|---|---|---|---|---|
| 1. H1/H2 同端口 | **有约束**：同一监听可同时服务 H1 与 h2c，但明文 H2 必须显式开启 `HttpServerOptions::h2c`；默认只按 H1 解析 | L2 + L0 | `.evidence/1-h1-h2.txt`；`pingora-core/src/apps/mod.rs:66,270,299` | 阶段 1 必须构建 `ProxyServiceBuilder` 并设置 `h2c = true`，否则 `curl --http2-prior-knowledge` 直接失败。TLS 侧走 ALPN，与 h2c 无关 |
| 2. 请求/响应 body 钩子 | **成立**：按块可见、非强制整体缓冲、可无损透传；块边界不稳定，不能当契约 | L2 | `.evidence/2-body.txt`、`2-body-hooks.txt`；8 MiB 上传 128×65536 字节，摘要一致；1 MiB chunked 收 32 块（末块 299 B） | 过滤器必须按流式状态机实现（跨块残包自留），不得假设 64 KiB 或 1:1 块映射。请求头先发给上游、body 随后流式转发 |
| 3. 路由元数据晚到 | **成立（这就是默认行为）**：`upstream_peer` 在上游连接/请求头阶段执行，body 一个字节都还没读；在过滤器里读完 body 则**不会**被重放，上游挂起 | L2 + L0 | `.evidence/3-late-metadata.txt`（elapsed_ms=0 连接、约 8 s 收完 body）；`.evidence/10-body-consumed.txt`；`proxy_h1.rs:122`、`proxy_h1.rs:483` | 依赖 body（如 `model`）选上游的设计不能走"过滤器读 body 再转发"的朴素路径；阶段 1 必须先设计受控的缓冲/重放或非 body 路由，并单独验证 |
| 4. SSE 转发 | **成立**：事件级写回，H1 与 H2 客户端都按 mock 的 200 ms 节奏收到；任意字节分片重组后内容一致 | L2 | `.evidence/4-sse.txt`：5 个事件到达间隔 201/202/201/202 ms，`[DONE]` 完整；mode=1/3/rand 三种分片 sha256 全等 | 阶段 1 透传 SSE 无需额外 flush API；不得开启响应压缩。下游块边界会被 TCP 合并，解析器仍必须跨块保存状态 |
| 5. 慢客户端背压 | **成立**：上游写入被反压，网关内存有界 | L2 | `.evidence/5-backpressure.txt`：32 MiB / 2 MiB·s⁻¹，mock 每 8 MiB 用时 1.8/4.1/4.5/4.3 s；网关 RSS 采样在 7 MB 量级保持恒定（10 次） | 不需要自建缓冲队列；禁止为"提速"在过滤器里 collect 整个响应体，否则有界性由你负责 |
| 6. 取消传播 | **成立**：客户端断开后上游连接在 ~70 ms 内被关闭 | L2 | `.evidence/6-cancel.txt`：网关 504 ms 记为 `ConnectionClosed / Downstream`，mock 570 ms 写失败 EPIPE | 取消不需要手工转发；但要把 `Downstream ConnectionClosed` 归为 Cancelled，不能记 500，也不要重试 |
| 7. 连接池作用域 | **成立**：池属于 service 实例（进程级，`Arc` 被所有 worker 共享），不是 per-worker；H1 池每条连接同时只承载一个请求 | L2 + L0 | `.evidence/7-pool.txt`、`7b-pool-workers.txt`：5 次独立下游连接复用同一上游端口；4 worker 下 8 并发→8 条连接，随后 8 次顺序请求复用这 8 条；`services/listening.rs:299`、`upstreams/peer.rs:777` | 连接数 ≈ 并发请求数（H1 不复用多路）；不要假设"每 worker 一份池"。下游 H1/H2 共享同一上游池；池键由 `HttpPeer` 哈希（地址/scheme/SNI/证书/校验开关）决定 |
| 8. graceful 退出 | **有约束**：SIGTERM 后立即停止监听，但在途流只被"宽限期"保护；宽限期是固定 sleep，不是等空闲；默认 300 s | L2 + L0 | `.evidence/8-graceful.txt`：`grace=3` 时 10 s SSE 在 17 个事件处被截断、进程约 3.1 s 退出；0.4 s SSE 完整收尾；默认日志 `grace period 300s starts` 且 8 s 后进程仍在；`server/mod.rs:63,809` | `nico-server` 必须显式设置 `grace_period_seconds` 与 `graceful_shutdown_timeout_seconds`；要 drain 长 SSE，宽限期必须覆盖最长流时长，或用 499/事件化收尾 |
| 9. WebSocket / gRPC | **有约束**：H1 WebSocket 透明代理成立（101 + 双向帧）；H2 明文 gRPC 透明代理成立（上游需显式 h2 + trailers 透传）。WS over H2、gRPC 双向流未验证 | L2 + L0 | `.evidence/9-ws-grpc.txt`：text/binary/ping/close 全部回显；`grpc-status: 0`/`grpc-message` 到达 curl，`upstream_response_trailer_filter` 收到同一对；`server.rs:787`（H2 下 `is_upgrade_req()` 恒 false） | 阶段 1 只能承诺"WS 透明字节转发（H1）"和"gRPC unary/服务端流（H2）"；不要承诺 WS 消息感知或 gRPC 转码/反射 |
| 10. 大 body（8 MiB，关键字段在末尾） | **成立**：请求与响应都字节无损，摘要一致；未触发任何默认大小限制 | L2 | `.evidence/2-body.txt`：上传/下载 sha256 均与对端声明一致 | 必须自己设上限（本 PoC 8 MiB 通过，说明"没有默认限制"≠"有保护"）。`Expect: 100-continue` 原样转发，上游的 100 会进入 `upstream_response_filter`（status=100），过滤器必须按 `is_informational()` 分支 |

补充事实（同样影响阶段 1）：

- **默认不加 `server` 头。** H1/H2 响应里都没有 `Server`；需要自己加。
- **H2 下游 → H1 上游会自动补 `host`。** `proxy_h1.rs:55-63` 从 `:authority` 合成 `Host`；
  实测 `curl --http2-prior-knowledge /hello` 的上游请求是 `version=HTTP/1.1` 且带
  `host: 127.0.0.1:18080`。
- **上游协议默认是 H1。** 只有 `peer.options.alpn = ALPN::H2` 时才会走 h2c
  （`connectors/http/mod.rs:106-130`）。gRPC 路由必须显式设置。
- **1xx 会进 `upstream_response_filter`。** 该过滤器对 100 Continue 也被调用（`status=100`,
  `headers={}`），无条件下插入响应头会污染 1xx。
- **压缩模块默认注册但关闭。** `init_downstream_modules` 默认加
  `ResponseCompressionBuilder::enable(0)`（`proxy_trait.rs:71`）；对 SSE 不要开启。

## 2. 环境与版本

| 项 | 值 |
|---|---|
| 机器 | macOS 27.0.0 arm64（Darwin 27.0.0, Apple Silicon） |
| Rust | `rustc 1.99.0`（aarch64-apple-darwin），edition 2024 |
| Pingora | `=0.9.1`（features `proxy`；默认 features 为空） |
| HTTP 客户端 | `curl 8.7.1` + nghttp2 1.69.0（支持 `--http1.1` / `--http2-prior-knowledge` / `--limit-rate` / `--trace-time`） |
| 额外构建依赖 | `cmake`（`flate2/zlib-ng` → `libz-ng-sys` 构建脚本；缺它直接失败） |
| TLS | 未启用。另测 `--features boringssl` 在 cmake 存在时可编译（约 45 s），但本 PoC 没有 TLS 监听 |
| 依赖图 | `cargo tree -d` 在 poc 图中没有重复的 `h2`/`hyper`/`tokio`/`rustls`/`tonic`（`.evidence/cargo-tree-d.txt` 为空） |

构建与测试（硬性门槛，已通过）：

```text
$ cargo build --release --all-features
    Finished `release` profile [optimized] target(s)
$ cargo test --all-features
test tests::base64_pads_correctly ... ok
test tests::ws_accept_matches_rfc6455_example ... ok
```

## 3. 逐项证据

### 3.1 H1/H2 同端口（结论：有约束，L2 + L0）

命令（脚本 §1）：

```bash
curl -sS --http1.1 -D - -o /dev/null http://127.0.0.1:18080/hello
curl -sS --http2-prior-knowledge -D - -o /dev/null http://127.0.0.1:18080/hello
```

实测（`1-h1-h2.txt`）：

```text
HTTP/1.1 200 OK
x-spike-proto: h1
...
HTTP/2 200
x-spike-proto: h2
```

`x-spike-proto` 由 `early_request_filter` 里的 `session.is_http2()` 生成，不是猜的。
两个请求的 `x-mock-peer-port` 相同（示例 53364，每次运行不同），说明它们还复用了同一条上游连接。

**默认行为不是这样。** 未设置 `h2c` 时：

```text
$ curl -sv --http2-prior-knowledge http://127.0.0.1:18080/hello
* [HTTP/2] [1] OPENED stream for http://127.0.0.1:18080/hello
* Remote peer returned unexpected data while we expected SETTINGS frame.
  Perhaps, peer does not support HTTP/2 properly.
```

源码：`pingora-core/src/apps/mod.rs:66` 定义 `pub h2c: bool`；
`apps/mod.rs:270` 用 `map_or(false, |o| o.h2c)` 取默认值（即 false）；
`apps/mod.rs:277-299` 说明 TLS 连接忽略 h2c 并用 ALPN，明文连接只有在
`h2c == true` 时才 peek `PRI * HTTP/2.0` 前言并进入 H2 握手。

**影响**：`nico-http` 需要 `ProxyServiceBuilder::server_options`，且明文角色显式开 h2c；
TLS 角色由 ALPN 决定。二者不能靠同一个默认值蒙混。

### 3.2 请求/响应 body 钩子（结论：成立，L2）

命令（脚本 §2）：

```bash
# 8 MiB，末尾放 sentinel
curl -sS -X POST --data-binary @/tmp/spike-8mib.bin http://127.0.0.1:18080/upload
# 1 MiB chunked，无 content-length
curl -sS -X POST -H "Expect:" -H "Transfer-Encoding: chunked" \
  --data-binary @/tmp/spike-1mib.bin http://127.0.0.1:18080/upload
curl -sS -o /tmp/spike-dl.bin http://127.0.0.1:18080/download?mb=8
```

实测（`2-body.txt`）：

```text
upload fixture sha256: cabff7e820811fccc8c8586fd1c6d621446a86ad9820ee5eacd080ac427bb042
--- POST /upload, content-length, 8 MiB, sentinel at the end
{"len":8388608,"sha256":"cabff7e820811fccc8c8586fd1c6d621446a86ad9820ee5eacd080ac427bb042"}
--- POST /upload, chunked, 1 MiB (no content-length)
{"len":1048576,"sha256":"fbe8fc990d4770b55fcedfa0bf160fc168c322cb214e4786c173de06aecbd875"}
--- GET /download 8 MiB
mock-declared sha256: 0b4de58c3af2ac9bd933c937457d074e205c3731e74faad0daaf2fd190ce5c6b
client-received sha256: 0b4de58c3af2ac9bd933c937457d074e205c3731e74faad0daaf2fd190ce5c6b
```

钩子侧（`2-body-hooks.txt`，裁剪）：

```text
method=POST path=/upload elapsed_ms=31 req_chunks=128 req_bytes=8388608 req_last_chunk_bytes=65536
method=POST path=/upload elapsed_ms=4  req_chunks=32  req_bytes=1048576 req_last_chunk_bytes=299
method=GET  path=/download elapsed_ms=26 resp_chunks_upstream=129 resp_chunks_downstream=129 resp_bytes_upstream=8388608
```

判读：

- `request_body_filter` 每块调用一次，8 MiB 被读成 128 个 65536 字节块（读取缓冲 64 KiB），
  最后一块按实际长度截断；chunked 输入的块序列与下游无关（32 块、末块 299 B）。
- `upstream_response_body_filter` 与 `response_body_filter` 的上行/下行块计数一致
  （128/128），说明默认路径不重组、不缓冲。
- 块数量会随 socket 时序变化（同一 8 MiB 下载在两次运行中分别是 129 和 128 块），
  **块边界不是契约**。
- 请求头/响应头阶段与 body 阶段分离：`upstream_request.connected` 的
  `first_body_chunk_seen=false`。

源码：`proxy_trait.rs:148`（`request_body_filter`）、`:451`（`upstream_response_body_filter`）、
`:472`（`response_body_filter`）。

### 3.3 路由元数据晚到（结论：成立，默认就是"先连上游再收 body"；L2 + L0）

命令（脚本 §3）：1 MiB body 以 128 KiB/s 限速发送，耗时 8 s，并抑制 `Expect`。

实测（`3-late-metadata.txt`）：

```text
--- mock: request head long before the body is complete
mock.req.head t_ms=631  ... method=POST target=/upload
mock.req.body t_ms=8636 ... body_bytes=1048576 body_sha256=fbe8fc99...
--- gateway: upstream connected at elapsed_ms=0, body streamed for ~8s
spike.upstream.connected ... elapsed_ms=0 reused=true ... first_body_chunk_seen=false
spike.done ... elapsed_ms=8013 ... req_chunks=18 req_bytes=1048576  (mock 侧 8 s 才收完)
```

即：`upstream_peer` / 连接/上游请求头都在第 0 ms 完成，body 在此之后持续 8 s 才到齐。
所以"等 body 看完再选上游"不是默认能力。

**反例（重要）**：在 `upstream_peer` 里把 body 读完再返回 peer（`/peek`），0.9.1 的默认转发路径不会重放这些字节：

```text
--- POST /peek: upstream_peer drains the body, then proxies
curl: (28) Operation timed out after 3007 milliseconds with 0 bytes received
spike.req.peeked ... peeked_bytes=1048576 peeked_sha256=fbe8fc99...
spike.upstream.request ... headers={"content-length": "1048576", ...}
mock.conn.error ... error=eof with 0 of 1048576 body bytes
```

上游收到请求头（带 `content-length: 1048576`）后一个 body 字节都没收到；把 curl 的超时去掉后
连接挂了 2 分钟直到客户端放弃。源码侧的解释：`proxy_h1.rs:122` 先写上游请求头，
`proxy_h1.rs:483` 的 `get_retry_buffer()` 只在**重试**时用于重放；`v1/server.rs:586-596` 的
`read_body_bytes` 只把读到的数据写入 retry buffer，不改变"首个上游尝试从下游流读取 body"的事实。

**影响**：AI 路由若需要 body 内的 `model`，阶段 1 必须在 `nico-http` 明确选一条受支持的路：
(a) 路由不依赖 body；(b) 在进入 ProxyHttp 之前完成缓冲并改写请求（自定义 forwarding / subrequest）；
(c) 有界扫描并延迟上游选择，但要解决重放。这个选择是本 PoC 给出的**最大未决设计点**。

### 3.4 SSE 转发（结论：成立，L2）

命令（脚本 §4）：

```bash
curl -sS -N --trace-time --trace-ascii /tmp/sse.trace -o /tmp/sse.txt \
  "http://127.0.0.1:18080/sse?n=5&delay_ms=200"
curl -sS -N --http2-prior-knowledge --trace-time -o /tmp/sse-h2.txt \
  "http://127.0.0.1:18080/sse?n=3&delay_ms=200"
```

H1 客户端到达时间（`4-sse.txt`）：

```text
10:15:57.113144 <= Recv data, 20 bytes (0x14)   # data: event-0
10:15:57.314093 <= Recv data, 20 bytes (0x14)   # +201 ms
10:15:57.516225 <= Recv data, 20 bytes (0x14)   # +202 ms
10:15:57.717332 <= Recv data, 20 bytes (0x14)   # +201 ms
10:15:57.919453 <= Recv data, 20 bytes (0x14)   # +202 ms
10:15:58.121470 <= Recv data, 19 bytes (0x13)   # data: [DONE]
```

H2 客户端到达时间：

```text
10:15:57.113987 <= Recv data, 15 bytes (0xf)
10:15:57.315021 <= Recv data, 15 bytes (0xf)
10:15:57.516272 <= Recv data, 15 bytes (0xf)
10:15:57.718122 <= Recv data, 14 bytes (0xe)   # [DONE]
```

分片重组（mock 把同一事件按 1 B / 3 B / 伪随机 1–7 B 写成独立 HTTP chunk）：

```text
mode=1    sha256=99035c273eac0b8912104ba35fe96e5a1736b3b1ae436de113ba3e1c13b660c5
mode=3    sha256=99035c273eac0b8912104ba35fe96e5a1736b3b1ae436de113ba3e1c13b660c5
mode=rand sha256=99035c273eac0b8912104ba35fe96e5a1736b3b1ae436de113ba3e1c13b660c5
expected  sha256=99035c273eac0b8912104ba35fe96e5a1736b3b1ae436de113ba3e1c13b660c5
```

钩子侧：mode=1 的 44 字节被 `upstream_response_body_filter` 记为 44 个 1 字节块
（`resp_chunks_upstream=44`），而客户端只看到 9 次 `Recv data`——**上游 chunk 边界在钩子里可见，
下游线缆上会被合并**。所以 SSE 解析器必须按字节流处理，输出侧不要依赖"一次钩子=一次写出"。

判读：事件级延迟 = mock 的 200 ms + ~2 ms，没有额外缓冲；`[DONE]` 完整送达；不需要显式
flush API。注意 `init_downstream_modules` 默认注册了压缩模块但关闭（`proxy_trait.rs:71`），
SSE 路由不要开启压缩。

### 3.5 慢客户端背压（结论：成立，L2）

命令（脚本 §5）：mock 写 32 MiB 无延迟，客户端 `--limit-rate 2m`。

实测（`5-backpressure.txt`）：

```text
downloaded=33554432 time=16.013948
rss samples: rss_kb=7184 rss_kb=7424 rss_kb=7424 ... (10 次，稳定在 7 MB 量级)
--- mock write progress (each mark is 8 MiB, blocked by the slow client)
mock.flood.progress written=8388608  elapsed_ms=1777
mock.flood.progress written=16777216 elapsed_ms=5833
mock.flood.progress written=25165824 elapsed_ms=10290
mock.flood.progress written=33554432 elapsed_ms=14578
```

判读：mock 的 `write_all` 被阻塞，8 MiB 一段的耗时约等于客户端消费速度（2 MiB/s）；
网关 RSS 全程不涨。结论是"上游被反压、网关内存有界"，不是"某个具体字节数上限"。

### 3.6 取消传播（结论：成立，L2）

命令（脚本 §6）：

```bash
curl -sS --max-time 0.5 -o /dev/null http://127.0.0.1:18080/cancel
```

实测（`6-cancel.txt`）：

```text
curl: (28) Operation timed out after 505 milliseconds with 140 bytes received
mock.cancel.tick        tick=10 elapsed_ms=521
mock.cancel.write_failed ticks=11 elapsed_ms=570 error=Broken pipe (os error 32)
spike.done elapsed_ms=504 status=200 ... error=Some(Error { etype: ConnectionClosed,
  esource: Downstream, ... "Prematurely before response body is complete" })
```

判读：客户端在 505 ms 断开，网关立刻结束请求并关闭上游；mock 在下一次写（50 ms 周期）看到
EPIPE。没有重试、没有继续拉取。

### 3.7 连接池作用域（结论：成立，L2 + L0）

命令（脚本 §7、§7b）。

单 worker，5 次独立下游连接 + H1/H2 各一次（`7-pool.txt`）：

```text
--- 5 sequential requests, separate downstream connections
x-mock-peer-port: 53364   (x5, 同一端口)
--- h1 and h2 downstream share the same upstream pool
x-mock-peer-port: 53364   (H1)
x-mock-peer-port: 53364   (H2)
--- upstream connections opened in total (mock view)
2
```

4 worker、8 并发 + 随后 8 次顺序请求（`7b-pool-workers.txt`）：

```text
workers that handled the burst: ThreadId(2) x2, ThreadId(3) x2, ThreadId(4) x3, ThreadId(5) x1
upstream ports used by the burst:
peer_port=53372 53375 53377 53379 53381 53383 53385 53387   (8 条连接，互不相同)
--- 8 sequential requests reuse exactly those 8 ports (到达顺序不同，集合相同)
x-mock-peer-port: 53372 53379 53375 53377 53383 53381 53385 53387
```

源码：`pingora-core/src/services/listening.rs:299` 把 `app_logic` 放进单个
`Arc`，每个 worker 线程拿 `Arc::clone` —— 一个 `HttpProxy` 实例（因此一个 `Connector`、一套池）
被所有 worker 共享。池键是 `HttpPeer` 的 `Hash`（`upstreams/peer.rs:777-800`：地址、scheme、
proxy、SNI、client cert、`verify_cert`/`verify_hostname`、`group_key`、h2 设置），h1 与 h2 分属
两个池（`connectors/http/mod.rs:106-130`）。

判读：连接数 ≈ 并发请求数（H1 不复用多路、无 pipelining）；顺序请求跨 worker 复用同一批连接。
"每 worker 一套池"是错的。

### 3.8 graceful 退出（结论：有约束，L2 + L0）

命令（脚本 §8）：`SPIKE_GRACE=3`，启动 10 s 的 SSE（50 × 200 ms），0.6 s 后 `kill -TERM`。

实测（`8-graceful.txt`）：

```text
sigterm->exit seconds: 3.05  (三次运行：3.05–3.08)
events received: 17
DONE received: 0
curl error: curl: (18) transfer closed with outstanding read data remaining
new request during grace: curl: (7) Failed to connect to 127.0.0.1 port 18084 ... (connection refused)
short stream events: 5 DONE: 1
INFO Graceful shutdown: grace period 3s starts
INFO Graceful shutdown: grace period ends
INFO All runtimes exited, exiting now
still alive 8s after SIGTERM with zero in-flight requests
INFO Graceful shutdown: grace period 300s starts
```

源码：`pingora-core/src/server/mod.rs:63` `const EXIT_TIMEOUT: u64 = 60 * 5;`，
`server/mod.rs:809` 用 `grace_period_seconds.unwrap_or(EXIT_TIMEOUT)`，随后是
`thread::sleep(exit_timeout)`（`server/mod.rs:811`），**不等空闲**。

判读：

- 停监听是即时的（宽限期内新连接被拒）。
- 宽限期内的短流可以完整收尾（0.4 s 流收到 `[DONE]`）。
- 超过宽限期的长流被截断（curl 报 18）；默认 300 s 意味着默认行为对长 SSE 很"宽松"，
  但进程在没有在途请求时也要睡满 5 分钟，运维语义不可接受。
- 阶段 1 必须显式配置两个参数，并把"最长流时长"作为宽限期的下界来设计。

### 3.9 WebSocket 与 gRPC（结论：有约束，L2；边界部分 L0）

WebSocket（H1 Upgrade，Python stdlib 客户端，`scripts/ws_client.py`）：

```text
10:16:33   | HTTP/1.1 101 Switching Protocols
10:16:33   | upgrade: websocket
10:16:33   | sec-websocket-accept: HCAAjSmoTtn3fw3qJIRqTctXgpY=
10:16:33 ws.accept_match=True
10:16:33 ws.echo opcode_sent=1 opcode_recv=1 fin=True len=10 match=True payload=b'hello-ws-1'
10:16:33 ws.echo opcode_sent=2 opcode_recv=2 fin=True len=8  match=True payload=b'\x00\x01...'
10:16:33 ws.pong opcode=10 payload=b'ping-payload'
10:16:33 ws.close opcode=8 payload=03e8
```

网关侧：`status=101 ... req_chunks=4 req_bytes=56 resp_chunks_upstream=4 ... was_upgraded=true`。
即 Upgrade 后帧被当作不透明字节流经过 body 钩子（4 个客户端帧 → 4 个请求块），
网关不解析帧，也不做消息级处理。上游请求保留了 `connection: Upgrade` / `upgrade: websocket` /
`sec-websocket-*`（`spike.upstream.request`）。

gRPC（H2 下游 + H2 上游 + trailers）：

```text
< HTTP/2 200
< content-type: application/grpc
< x-mock-h2: grpc
< grpc-status: 0
< grpc-message: spike-ok
--- upstream h2 request as seen by the mock
mock.h2.req method=POST uri=http://127.0.0.1:18080/grpc version=HTTP/2.0 body_bytes=10
  headers={"content-type": "application/grpc", "te": "trailers", "content-length": "10", ...}
--- trailers delivered to the gateway filter
spike.upstream.trailers trailers={"grpc-status": "0", "grpc-message": "spike-ok"}
spike.done path=/grpc proto=h2 status=200 resp_chunks_upstream=2 resp_bytes_upstream=38
```

判读与边界：

- 上游必须显式 h2：`peer.options.alpn = ALPN::H2`；默认 peer 无 ALPN 时
  `connectors/http/mod.rs:106` 判定 `h1_only`，即使下游是 H2 也走 H1 上游。
- trailers 双向可用：`upstream_response_trailer_filter`（`proxy_trait.rs:462`）看到上游 trailers，
  下游客户端也收到。响应体 2 个 DATA 帧、38 字节，未被重组。
- **WS over H2 未验证且源码上不支持**：`server.rs:787` 的 `is_upgrade_req()` 对 H2 恒为 false。
- gRPC 双向流、deadline 传播、trailers-only 响应、gRPC-Web 均未验证。

### 3.10 大 body（8 MiB，关键字段末尾）（结论：成立，L2）

命令与摘要见 §3.2：上传 fixture 末尾是 `TAIL-SENTINEL-8MiB`，mock 回传的
`{"len":8388608,"sha256":"cabff7e8..."}` 与本地 `shasum` 完全一致；下载 8 MiB 的
mock 声明摘要与客户端落盘摘要一致。chunked 无 `content-length` 的 1 MiB 上传也一致。

补充：`Expect: 100-continue` 被原样转发上游（`spike.req.head ... "expect": "100-continue"`）；
上游回 `100 Continue` 时它被转达给客户端（8 MiB 上传总耗时 31 ms；mock 不答 100 时同一
请求要 ~1.05 s，因为 curl 等 1 s 才自行发 body），并且 `upstream_response_filter` 会以 `status=100` 被调用一次：

```text
spike.upstream.response elapsed_ms=0 status=100 version=HTTP/1.1 headers={}
spike.upstream.response elapsed_ms=31 status=200 version=HTTP/1.1 headers={"content-type": "application/json", ...}
```

## 4. 对 `nico-http` 的直接约束

1. **监听配置**：明文角色必须 `h2c = true`；TLS 角色走 ALPN。`Server` 头需要自己加。
2. **过滤器形态**：`request_body_filter` / `upstream_response_body_filter` /
   `response_body_filter` 都是块级、非缓冲；SSE/WS 解析状态必须 per-stream 保存，
   不得假设块大小或块边界。
3. **1xx 分支**：`upstream_response_filter` 会对 100 Continue 调用；响应头改写必须先判断
   `status.is_informational()`。
4. **上游协议**：默认 H1；gRPC 路由必须把 peer 设为 `ALPN::H2`，否则静默降级为 H1。
5. **body 依赖路由**：不能"读 body 再转发"；阶段 1 必须先定方案（不依赖 body / 前置缓冲重写 /
   受控扫描 + 重放）并单独验证。
6. **连接池**：进程级、按 `HttpPeer` 哈希分池，H1 一连接一请求；容量规划按"并发请求数"算。
7. **取消**：下游断开自动向上游传播，分类为 Cancelled，不重试、不记 500。
8. **停机**：`grace_period_seconds` / `graceful_shutdown_timeout_seconds` 必须显式设置；
   宽限期是固定等待，长流会被截断。
9. **能力承诺**：只承诺实测覆盖的模式——WS 透明字节转发（H1）、gRPC unary/服务端流（H2）、
   SSE 透传；WS 消息感知、WS over H2、gRPC 双向流/转码不承诺。
10. **资源上限**：PoC 未触发任何默认 body 限制，说明限制必须由网关自己设（阶段 1 的
    `limits` 配置不是可选项）。

## 5. 仍需按后续版本/环境复验

| 项 | 状态 | 原因 / 后续验证方式 |
|---|---|---|
| TLS / ALPN 运行时行为（HTTPS、H2 over TLS、证书热加载、mTLS） | 未验证 | 本 PoC 无 TLS 监听；仅确认 `--features boringssl` 在 cmake 存在时可编译（L1）。阶段 1 需真机 TLS 监听 + `curl --tlsv1.3 --http2` |
| `pingora` 无 TLS feature 的生产可用性 | 已验证（明文） | 明文路径不需要任何 TLS feature；但生产角色若终止 TLS，需重新锁定 feature 与证书来源 |
| gRPC 双向流 / deadline / trailers-only / gRPC-Web | 未验证 | 本 PoC 只做 unary 形状（1 个请求消息、2 个响应消息 + trailers）；需要专门的 h2 客户端与流控场景 |
| WS over H2（RFC 8441 extended CONNECT） | 不成立（L0） | `server.rs:787` 对 H2 恒 false；若产品需要，必须换实现或自建 |
| 在过滤器内消费 body 的受支持重放方式 | 未验证（朴素路径失败，L2） | `/peek` 证明 `read_request_body()` 会吞掉 body；需调研 subrequest / custom forwarding / 自建 body 注入，并作为阶段 1 设计门 |
| 上游超时、重试预算、`Retry-After`、H2 downgrade 行为 | 未验证 | 本 PoC 未注入连接失败/429/5xx；阶段 1 用故障注入验证 commit 前重试与 AI 不可重试边界 |
| 压缩模块开启后的 SSE 行为 | 未验证 | 默认关闭；若开启 `ResponseCompressionBuilder`，需重测事件级 flush 与内存 |
| 连接池 idle 淘汰、DNS 变化、后端重启 | 未验证 | 本 PoC 只验证"复用发生"；需要长跑 + 上游重启场景 |
| Linux/容器环境差异（SO_REUSEPORT 分发、cmake 缺失、TLS 库） | 未验证 | 本次为 macOS arm64；构建需要 cmake，CI 镜像必须预装 |
| H2 下游多路并发 / flow-control 窗口 / `max_h2_streams` | 未验证 | 本 PoC 的 H2 请求都是单流；需并发 H2 客户端 + 大响应压测 |
| HTTP/1.1 pipelining（`patched_http1` feature） | 未验证 | 默认关闭且未启用该 feature；若要用必须单独取证 |
| 大 body 的实际默认上限 | 未验证（8 MiB 通过） | 未找到默认限制的边界；阶段 1 必须显式设上限并用超限请求验证拒绝路径 |
