# 评审清单修复记录（11-04-s1-2 收尾）

> 历史记录：为什么这些结构是这样，以及评审中哪一条判断被实测推翻。
> 最终设计见 `../design.md`；这里不重复描述。

## 起因

对 1.2 的改动做了一次「是否符合规范」的独立评审，逐条跑完五条基线、依赖边界、文件预算，
再按 `.trellis/spec/backend/` 的规则检查结构。结论是整体符合，但有五处需要处理。

## 五处修复与理由

### 1. provider 接缝只有文档，没有 API

`lib.rs` 与 `design.md` §6 都写「过滤器在 `upstream_peer` 之前改写目标」，但 `HttpContext`
当时只有 `target()` getter，1.4 无处下手。

**第一性原理**：数据面的不变量是「绝不自己猜地址」，而"谁理解了这个 target"应由 target 的
拥有者回答。于是把"请求去哪"抽成 `upstream::Destination { scheme, host, port }`：

- 文档 upstream 由 `Destination::from(&Upstream)` 得到；
- provider 由 `gateway-ai` 解析 `base_url` 后经 `HttpContext::set_destination` 记录。

**为什么不直接让过滤器返回 `Upstream`**：`gateway_core::Upstream` 的私有字段与
"只存在于已校验文档"的文档注释是它作为配置条目的不变量；为了一个派生值放宽它，会顺手允许
未校验的配置进入数据面。**为什么不返回 `HttpPeer`**：那会把 Pingora 的类型泄漏到 `gateway-ai`。

### 2. destination 被决策两次

原先 `upstream_peer` 用 `peer_for_upstream(ctx, name)` 查一次 upstream，`upstream_request_filter`
再用 `target_upstream(ctx)` 查第二次来算 `Host`。两次查找可以有不同结果（一次过滤后、一次
reload 后），且第二次是为了一个已经知道的值。

`proxy/forward.rs::destination` 现在只回答一次，结果记进 context，`Host` 重写读同一个值。

### 3. deadline 没有覆盖 body 与过滤器

评审时的判断是"下游 body 无界"，方向对、结论错：**慢上传原本就有界**，peer 的 `read_timeout`
（= 剩余预算）会在同一预算点收尾。实测显示：`Content-Length` 声明 2048、只发 8 字节的上传在
202 ms 后收到 `504 upstream_timeout`。

真正的缺陷是**分类**：客户端自己的慢被答成上游超时，运维会被送去查一个健康的上游。
由于 `request_body_filter` 是唯一**确知**"body 还没收完"的位置，它给出 `408 request_timeout`
（`Client`）；一个字节都不再发的上传仍由 peer 超时收尾为 504，这是"有界但不精确"的一格，
`body_bytes` 留在日志里用于区分。

过滤器链同样补上预算：`FilterChain::within_budget` 用 `timeout_at(ctx.deadline())` 包住每个 stage，
所以 1.3 的 JWT 拉 JWKS 不可能比它服务的请求活得更久。deadline 的两个派生事实（`expired`、
`deadline_error`）归 `HttpContext`，五个检查点共用，同一个请求只有一个耗时数字。

### 4. 进程装配放在了数据面 crate 里

`CancelWatch`/`run_args`/信号等待原先在 `gateway-http`，而 `directory-structure.md` 的
"where new code goes" 把 shutdown 归给 `gateway-server`；两份 spec 对同一职责给出相反归属。

**判断**：一个数据面库如果因为被链接就安装信号处理器，就无法嵌入测试或第二个前端。移动后
`gateway-http` 不再依赖 `tokio` 的 `signal` / `tokio-util`，`service.rs` 只剩
"`ServerConf` 派生 + builder + 监听"这一件事。

### 5. 小项

- `limits` / `upstream` 变 crate 私有，只导出 `Destination`：四个纯 helper 与 peer 构造没有
  外部消费者，"最小导出"是 spec 的明确要求。
- `Connection` 提名头超过 16 个直接 `400`：只删一半等于把剩下的连接语义泄漏给上游，
  而删除动作存在的理由正是防止这件事。
- 上游解析改 `tokio::net::lookup_host`：运行时默认 1 个 worker 线程，在 worker 上做阻塞解析会
  让同 service 的所有请求一起等；纯构造部分（`build`）保持同步、可单测。
- `x-request-id` 只接受可见 ASCII：它进每一行日志，控制字符等于允许调用方伪造日志记录
  （CWE-117）。不转义、直接换成生成值——转义后的 id 不再匹配调用方自己的日志，而那是保留它
  的唯一理由。
- `logging` 钩子的正文拆到 `proxy/record.rs`：`hooks.rs` 已到 460 行，spec 要求在 ~400 行就
  规划拆分，且"一个请求读什么"本来就是一个独立决定。

## 新增测试

| 行为 | 位置 |
|---|---|
| 过滤器记录的 destination 优先于文档 | `gateway-http/src/proxy/tests.rs` |
| provider target 无前端时 502 `provider_unavailable` | 同上 |
| `expired` / `deadline_error`（504） | `gateway-http/src/context/tests.rs` |
| 过滤器 stage 受预算约束（暂停时钟） | `gateway-http/src/filter/tests.rs` |
| `Connection` 上限：16 可服务、17 拒绝 | `gateway-http/src/proxy/tests.rs` |
| `Destination` 构造、TLS 拒绝先于解析、预算透传 | `gateway-http/src/upstream/tests.rs` |
| 400/408 加入错误映射三元组表 | `gateway-http/src/error/tests.rs` |
| 慢上传 → 408 + `Client` 日志 | `gateway-server/tests/http_limits.rs` |
| 取消 token → `GracefulTerminate` | `gateway-server/src/server/shutdown/tests.rs` |
| 真实进程在管线失败后的退出码 | `gateway-server/tests/failed_pipeline.rs` |
