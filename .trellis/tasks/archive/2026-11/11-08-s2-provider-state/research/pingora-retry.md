# Pingora 0.9.1 会话重建取证（implement.md 第 1 步）

证据全部来自本机 cargo registry 源码静态精读（文件:行号），未使用运行时探针（理由见 §5）。

路径缩写（均为 `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/` 下）：

- **PP** = `pingora-proxy-0.9.1/src`
- **PC** = `pingora-core-0.9.1/src`
- **PE** = `pingora-error-0.9.1/src`
- **nico** = `/Users/tomgs/workspaces/rust_space/nico/crates/nico-http/src`

---

## 1. 结论摘要

**能，但有一条硬边界。** 0.9.1 的 `process_request` 自带 `while retries < max_retries` 尝试循环，每轮重新调用 `upstream_peer` 并重建 upstream 会话（新 `HttpPeer` → 按地址寻池/新建连接，无跨尝试残留）；请求头每轮从下游请求重新克隆并重跑 `upstream_request_filter`，所以换 host/port 的目的地、逐尝试改写请求头都是框架原生支持的。客户端未收到任何字节之前有两个合法的重试时机：connect 失败（`fail_to_connect` + `set_retry(true)`）与响应头阶段失败（在 `upstream_response_filter` 返回 Err，它先于任何下游写执行）。

**硬边界**：请求体的二次写出依赖 Pingora 内建 retry buffer（64 KiB 常量，不可配置）。"客户端 body 已读完 + raw body > 64 KiB" 的重试——正是 converting 路由"provider 收到完整请求后答非 2xx"的常态——在下一 attempt 会发出**无 body 的请求**（`request_body_filter` 不会被调用，nico ctx 里的字节没有出口）。因此推荐：以内建循环为挂点，`ServerConf::max_retries` 提为上界、由 nico 在 ctx 计数；同时 design §2 必须补一条失败模式约束（分派后重试仅当 raw body ≤ 64 KiB，connect 阶段重试不受限；大 body fallback 需另走"自管剩余 attempt"的升级路径，见 §3.2）。

---

## 2. 源码证据

### 2.1 主循环结构（Q1）

**入口与循环。** `process_request`（PP/lib.rs:1148）顺序执行：`early_request_filter`（1157）→ 下游模块 request filter（1174）→ `request_filter`（1189，`Ok(true)` 即结束，1191-1206）→ `proxy_cache`（1216，缓存命中即结束；nico 不启用）→ `proxy_upstream_filter`（1226）→ **尝试循环**：

```rust
// PP/lib.rs:1275-1316（节选）
let mut retries: usize = 0;
while retries < self.max_retries {
    retries += 1;
    let (reuse, e) = self.proxy_to_upstream(&mut session, &mut ctx).await;
    match e {
        Some(error) => {
            let retry = error.retry();
            /* warn log if retry */
            proxy_error = Some(error);
            if !retry { break; }          // 不可重试 → 跳出，走 fail_to_proxy
        }
        None => { proxy_error = None; break; }  // 成功
    }
}
```

循环退出后：缓存 serve-stale（1322，nico 不启用）→ 有错误则 `fail_to_proxy`（1347）→ `finish`（1363，内含 `logging`，PP/lib.rs:532-534）。

**每轮一次 `upstream_peer`。** `proxy_to_upstream`（PP/lib.rs:401-485）开头调用一次（410-413）；`upstream_peer` 本身的 Err 直接 `return (false, Some(e))`（412）——**不经过 `fail_to_connect`，也不经过 `error_while_proxy`**，重试与否完全由 nico 自己在该错误上设置的 retry 位决定。取到 peer 后 `client_upstream.get_http_session(&*peer)`（415）拿连接，按协议分派到 `proxy_to_h1_upstream` / `proxy_to_h2_upstream` / `proxy_to_custom_upstream`（418-470），返回的错误统一交给 `error_while_proxy`（471-477）。`get_http_session` 失败则走 `fail_to_connect`（479-483）。

**`retries` 计数与 `max_retries`。** 0.9.1 **没有** `session.retries` 字段——计数是 `process_request` 的局部变量（PP/lib.rs:1275）。`self.max_retries` 来自 `ServerConf::max_retries`（PP/lib.rs:194、230；字段文档在 PC/server/configuration/mod.rs:134-137："maximum number of retries ... when an error is retry-able ... defaults to 16"，常量 `DEFAULT_MAX_RETRIES = 16` 在第 35 行）。循环体 `retries += 1` 先于首次尝试（PP/lib.rs:1281），所以 **`max_retries: N` = 至多 N 次上游尝试**；nico 现在的 `max_retries: 1`（nico/service.rs:45，注释 33-35 已正确理解语义）= 恰好一次尝试，内建重试整体关闭。

### 2.2 会话重建（Q2）

**每次尝试一个新 upstream 会话。** `proxy_to_upstream` 每轮拿新的 `HttpPeer`（PP/lib.rs:410）后经 HTTP 连接器取会话：PC/connectors/http/mod.rs:66-133（h1-only 判定 106-108，取 h1 会话 110）→ PC/connectors/http/v1.rs:35-42：`transport.get_stream(peer)`（池命中或新建 TCP）→ `HttpSession::new_with_options(stream, peer)`。**换 host/port = 不同 peer = 不同复用键 = 新建连接**；TLS/SNI 状态是建连时从 peer 自带配置生成的（本项目 `TLS_BACKEND_ENABLED=false`，nico/upstream.rs:47 直接拒绝 TLS 上游，无 TLS 重建问题）。上一轮的 client session 只在可复用时回池（PP/lib.rs:423-428），错误路径 `client_reuse=false` 即丢弃。

**请求头每轮重新克隆。** H1：`proxy_1to1` 开头 `let mut req = session.req_header().clone()`（PP/proxy_h1.rs:48），随后 sanitize（65）、**`upstream_request_filter`（81-90）**、authority 校验（93-110）、framing finalize（112）、`write_request_header`（122）。H2 同构：克隆在 PP/proxy_h2.rs:133，`upstream_request_filter` 在 173-181。所以：(a) 请求头对每轮尝试都是干净副本，**可重复写、逐轮可改**；(b) nico 的 `upstream_request_filter`（nico/proxy/hooks/request.rs:197）每轮执行一次，`llm::apply_upstream_request` 读 ctx 即可实现逐 attempt 的 path/headers。

**跨尝试保留的状态**（都在 `Session` 上，PP/lib.rs:572-610）：下游会话本体（请求头、body reader、retry buffer、keepalive 设置）、cache ctx、`upstream_compression` ctx、`h1_upgrade_request_status`、以及 nico 的 `HttpContext`。upstream 侧的一切随新 client session 重建。nico 的 `ctx.destination()`（nico/context.rs:172、181）由 `upstream_peer` 每轮 `set_destination` 覆盖，`upstream_request_filter` 读同一值（nico/proxy/hooks/request.rs:185-188、214-236）——每轮自洽，无需清理。

**下游会话边界**：请求头永不被消费（每轮克隆源是同一个 `session.req_header()`）；请求体 reader 的推进是累积的（见 §2.3），body 读完即为 `is_body_done()=true` 并跨尝试保持。

### 2.3 请求体重发（Q3）

**0.9.1 的内建回放机制就是 retry buffer**；不存在 `session.replay()`、`as_downstream().as_message_mut()`、`BufferedRequest`（全库 grep 无匹配，旧版 API 已移除）。

- **开启与容量**：每次尝试的 duplex 循环启动前 `session.as_mut().enable_retry_buffering()`（PP/proxy_h1.rs:145；H2 PP/proxy_h2.rs:297）。容量常量 `BODY_BUF_LIMIT = 1024 * 64`（PC/protocols/http/v1/common.rs:32；H2 PC/protocols/http/v2/server.rs:49），**不可配置**。
- **累积**：下游每读一个 chunk，`read_body_bytes` 把**原始**字节写进 buffer（PC/protocols/http/v1/server.rs:586-596）。注意累积发生在 nico 的 `request_body_filter` 之前——buffer 里是客户端原始字节，不是 nico 转换后的字节。buffer 在新请求开始时清空（`init_body_reader`，PC/protocols/http/v1/server.rs:1118-1123），跨 keepalive 无残留。
- **回放**：每次尝试的 `proxy_handle_downstream` 开头（PP/proxy_h1.rs:483-499）：

  ```rust
  let buffer = session.as_ref().get_retry_buffer();
  // retry, send buffer if it exists or body empty
  if buffer.is_some() || session.as_mut().is_body_empty() {
      ... self.send_body_to_pipe(session, buffer, downstream_state.is_done(), ...).await?;
  }
  ```

  `send_body_to_pipe`（PP/proxy_h1.rs:1020-1072）先跑下游模块 filter，再跑 **nico 的 `request_body_filter`**（1044-1046），再入 pipe 写上游。H2 对称（PP/proxy_h2.rs:528-541）。即：**重试轮里 nico 的 `request_body_filter` 会被再次调用**，输入是攒下的原始 body（body 读完时为一整块、`end_of_stream=true`）或"已读前缀 + 后续客户端新读"。
- **截断语义**：超 64 KiB 时 `FixedBuffer::write_to_buffer` 丢弃全部内容并置 `truncated`（PC/protocols/http/body_buffer.rs:23-33）；`get_retry_buffer` 对 truncated 返回 `None`（PC/protocols/http/v1/server.rs:1167-1175）；`retry_buffer_truncated()`（1155-1159）就是框架留给 `error_while_proxy` 的判断输入（默认实现据此强制不重试，PP/proxy_trait.rs:596-601）。
- **不可重入的空档（本取证最重要的负结论）**：当 `body_done=true` 且 buffer 被截断（raw > 64 KiB）时，PP/proxy_h1.rs:486 的条件 `buffer.is_some() || is_body_empty()` 两个分支皆 false → **没有任何初始 body 发送**；select 循环里读分支的守卫 `downstream_state.can_poll()` 对 `ReadingFinished` 仍为 true（PP/proxy_common.rs:433-435、423-431），于是调 `read_body_or_idle(true)`——body 已完成时它只做 idle 探测并**永久 pending**（PC/protocols/http/v1/server.rs:1217-1265，1218 行 `no_body_expected || is_body_done()` 进 idle 分支）→ `request_body_filter` 在这一轮**不会再被调用** → 上一轮已写出的请求头（带 content-length/chunked 声明）后面**一个 body 字节都没有**。nico ctx 里的字节没有出口：`request_body_filter` 是唯一的 body 注入缝，而这个空档里它不被调用。

**nico ctx 重发的可行形状**（对 ≤ 64 KiB 或"body 未读完"两种情形完全可行）：

- 时序：每次尝试内 `upstream_request_filter`（写头前）→ `write_request_header`（PP/proxy_h1.rs:122）→ duplex 循环 → 首个 body 任务经 `send_body_to_pipe` → nico `request_body_filter`。即 `upstream_request_filter` 是**每轮尝试的边界标记**，先于该轮一切 body 任务。
- `as_upstream`/重入性：nico 拿到的始终是 `Session`（下游会话包装）；upstream 会话由框架私有持有（`proxy_to_upstream` 是私有方法，PP/lib.rs:401），hook 无法、也不需要直接触达。重入点只有 `request_body_filter` 的输出参数 `*body`。
- converting 路由现状（nico/proxy/hooks/llm.rs:109-161）：攒 chunk → end_of_stream 时 `prepare()` 一次 → `*body = prepared`。若走内建重试，第二轮会**再次 prepare**（同一份原始字节重放经过同一 filter），违背 design 不变量 1（canonical 只解码一次）且双耗 CPU。修订：`record_prepared`（nico/context/llm.rs:84-87，目前只存 streaming/model 两个事实）扩展为同时保存 canonical/逐 attempt 编码；`request_body_filter` 检测"已 prepared"状态时**不再走攒-prepare 分支**，直接用 ctx 里的 per-attempt 编码替换 `*body`（与 design §2"每次尝试由该 canonical 重新编码"一致）。
- **attempt ≥2 时 ctx 累积的双计风险**：若 attempt 1 在 body 未读完时失败，buffer 回放的前缀字节已经 push 过一次；重试轮这些前缀会再经过 `request_body_filter`。因此 `upstream_request_filter`（每轮边界）必须重置 ctx 的 body 累积状态（"本轮 body 流从零开始，回放前缀即将重放"）。

### 2.4 两个放弃时机（Q4）

**(a) connect 阶段失败。**

- 调用点：仅 `get_http_session` 失败时（PP/lib.rs:479-483）：`e.as_up()` → `fail_to_connect(session, &peer, ctx, e)` → 返回 `(false, Some(new_err.into_up()))`。**`upstream_peer` 自己的 Err 不经过它**（410-413），DNS 解析失败（nico 在 `upstream::peer()` 里做 `lookup_host`，nico/upstream.rs）想换目标必须由 nico 在该错误上直接置 retry 位。
- 返回值语义：0.9.1 的 `fail_to_connect` 返回 `Box<Error>`（不是老版本的 bool），文档明说"由用户标记 e 是否可重试"（PP/proxy_trait.rs:605-621）。connect 错误默认 **retry=false**：`Error::create` 的 retry 除非 cause 链携带否则一律 `Decided(false)`（PE/lib.rs:209-217），TCP connect 路径全程无人 `set_retry`（PC/connectors/l4.rs:158-217 只包 ConnectTimedout/ConnectError；CONNECT 代理路径 329-332 甚至显式 `decide_reuse(false)`）。**nico 必须实现 `fail_to_connect` 并 `e.set_retry(true)`，否则 connect 失败永不重试**（nico/proxy/hooks.rs 目前没有实现这个 hook）。
- 返回 true（即 retry 位为 true）后：主循环回到 `proxy_to_upstream`（PP/lib.rs:1283）→ 重新调 `upstream_peer`（410）→ **换 peer 重试成立**；`fail_to_proxy` 只在循环耗尽或不可重试时触发（1347）。此刻响应管线尚未开始，客户端必然零字节。

**(b) 响应头已读、未写下游时失败。**

- 调用点：`proxy_to_h1_upstream`/h2/custom 返回 Err 之后，统一在 PP/lib.rs:471-477 调 `error_while_proxy(&peer, session, e, ctx, client_reused)`，其返回值就是循环看到的错误；`error.retry()` 为 true → 循环再来一轮 → **回到"重新取 peer"**（1283→410），而不是 `fail_to_proxy`。
- nico 覆写了这个 hook（nico/proxy/hooks/response.rs:133-149）并显式 `error.set_retry(false)`（144-148，注释援引 max_retries=1）——覆写即**替换默认策略**（PP/proxy_trait.rs:585-586），重试决策完全归 nico。
- **客户端零字节的确认**：响应任务的写出口只有 `process_upstream_tasks` 尾部的 `session.write_response_tasks(filtered_tasks)`（PP/proxy_h1.rs:436），而过滤器先于它执行（397-429：`upstream_filter` → nico `upstream_response_filter` 在 PP/lib.rs:497-503）。nico 在 `upstream_response_filter` 返回 Err（如 classify 出 Attempt(next)）时，`?` 在 417 行传播，**先于 436 行的任何下游写**。上游读错误（PP/proxy_h1.rs:315-321）变成 `HttpTask::Failed` 入 pipe，经 423-427 或 436 → `response_duplex_vec` 对 Failed 的处理是"flush 已缓冲数据并返回 Err"（PC/protocols/http/v1/server.rs:1579-1587）——flush 前若从未写过响应头则客户端零字节，**若 header 任务已写出则客户端已有字节**。
- **发头后不得重试（框架的兜底与边界）**：下游会话对二次写头是静默丢弃（`prepare_response_header`，PC/protocols/http/v1/server.rs:830-837 "Respond header is already sent"）；`fail_to_proxy` 的错误应答对已写响应是无操作（PC/protocols/http/server.rs:629-636）。即框架保证不双重写头，但**不阻止**重试轮把第二份响应头之后的字节流写坏——重试开关必须由 nico 用 `session.response_written()`（PC/protocols/http/v1/server.rs:681-683，nico/response.rs:176 已在用）作单行门：`response_written().is_some() → retry=false`。

**(c) 其他客户端零字节的重试点**（顺带）：`upstream_request_filter` Err（PP/proxy_h1.rs:87-89，先于 122 行写头）、`connected_to_upstream` Err（PP/proxy_h1.rs:214-227）。`request_body_filter` Err 发生在 body 阶段——header 早已写出，客户端虽未见 body 字节但请求头已写，属于"未提交但已单向"状态；走内建重试轮协议上安全（下一轮重新写头），但 nico 的 body 错误（超限/超时）语义上不应重试。

### 2.5 取消 / graceful（Q5）

- **新请求**：`handle_new_request` 的读请求 select 挂了 `await_shutdown`（PP/lib.rs:332-339）——shutdown 触发即丢弃连接。`process_new_http` 在 shutdown watch 已触发时禁用 keepalive（PP/lib.rs:1582-1585）。`http_cleanup` 置 flag 并唤醒所有等待者（PP/lib.rs:1598-1602；`await_shutdown` 实现 306-320）。
- **在途尝试**：尝试循环（PP/lib.rs:1280-1316）与 `proxy_to_upstream` 内**没有任何 shutdown 检查**——在途重试不会被优雅关闭打断，请求会跑完当前轮并正常收尾。
- **硬边界**：优雅关闭 = `grace_period_seconds` 的线程 sleep（PC/server/mod.rs:800-813），之后 runtime `shutdown_timeout`（`graceful_shutdown_timeout_seconds`，缺省 5s，PC/server/mod.rs:816-824）强制收掉任务。nico 已显式配置这两个窗口（nico/service.rs:42-43）。
- nico 侧可用 `session.is_process_shutting_down()`（PP/lib.rs:968-970）在 `upstream_peer` / retry 决策点主动放弃剩余 attempt，避免把注定被强杀的尝试发给新目标。

---

## 3. 风险与设计修订建议

### 3.1 对 nico 现有假设的逐条核对

| nico 现有代码 | 取证结论 | 需要 |
|---|---|---|
| `service.rs:45` `max_retries: 1` | 循环 = 恰好一次尝试 | 提为链展开上界（如 `retry_budget × 链长`，或固定 16 作保险丝）；真正的次序控制在 ctx 里 |
| `response.rs:133-149` `error_while_proxy` 恒 `set_retry(false)` | 该 hook 是重试决策点；覆写即全权负责 | 改为：有下一 attempt 且 `response_written().is_none()` 且非下游断连 → `set_retry(true)`；否则维持 false |
| `hooks.rs` 未实现 `fail_to_connect` | connect 错误默认 retry=false | 必须实现：有下一 attempt → `set_retry(true)`（客户端此时必然零字节） |
| `upstream_peer` Err（DNS 失败等）不经过 `fail_to_connect` | PP/lib.rs:412 直接返回 | nico 自己在该错误上置 retry 位（或在 ctx 内直接推进 plan 后再返回 peer/Err） |
| `llm::request_body`（llm.rs:109-161）每次 end_of_stream 都 `prepare` | 重试轮会二次 prepare | "已 prepared"状态短路：直接输出 ctx 存的 per-attempt 编码 |
| `ctx.destination()` 每轮由 `upstream_peer` 覆盖 | 每轮自洽 | 无需改；`upstream_request_filter` 继续只读 |
| `upstream_response_filter`（response.rs:44-90）无条件 `install_transform` | 非 2xx 判定就在这里发生，且这里是零字节重试点 | classify 前置：2xx → 现状；非 2xx → 问前端 classify → Serve（翻译错误头，现状）或 Attempt(next)（返回 retry=true 的 Err） |
| 不变量 2（发头即单行门） | 框架只防双写头，不防重试 | 在 retry 决策处用 `session.response_written()` 断言/短路，作为可测试的单行门 |

### 3.2 design.md §2 需要修订为——

在 §2 的尝试循环图之后增加一条**字节级约束**（新失败模式行）：

> **请求体重放走 Pingora 内建 retry buffer（64 KiB 上限，不可配）**。尝试循环必须区分两种失败点：
> 1. **body 未分派**（connect/写头失败，或 converting 路由尚未攒完 body 时的失败）：客户端字节未消费或仅部分消费，重试轮从 retry buffer 前缀 + 客户端续读重建，任意 body 大小安全；
> 2. **body 已分派后失败**（classify 到非 2xx、响应读中断）：重试轮的 body 只能来自 retry buffer，因此**仅当客户端 raw body ≤ 64 KiB 安全**；更大的 body 在重试轮会发出无 body 的请求（PP/proxy_h1.rs:486 双 false + `read_body_or_idle(true)` 永久 pending，见 §2.3）。
>
> 因此：`retry_budget`/fallback 对"body 已分派后失败"生效的前提是 **raw body ≤ 64 KiB**；超限时非 2xx classify 直接落 `Serve`（按现状翻译错误头应答）。connect 阶段重试不受此限。

配套设计修订（若"大 prompt 的分派后 fallback"是硬需求，升级路径）：

- **方案 B（自管剩余 attempt）**：在 attempt 0 的响应头阶段（`upstream_response_filter`，零字节时点）classify 出 Attempt(next) 后，对 raw > 64 KiB 的 converting 请求由 nico 用自己的连接器（`upstream::peer()` + TCP）完成剩余 attempt：自写该 attempt 的请求头与 ctx 里的 prepared body、自读响应头、classify；最终结果按"合成响应"路径写回客户端（`Session::write_response_header/body` 是公开 API，failure.rs 的 `write_respond` 已在用），同时让 attempt 0 以 Err 结束（走 `error_while_proxy`/`fail_to_proxy` 的既有收尾）。代价：最终 attempt 的响应体不再经过 Pingora 的 response 管线——流式 2xx 答案需要 nico 自行转发（驱动 transform + 预算 + 分帧），复杂度显著；错误回答本就是 buffered 翻译（llm.rs:188-193 注释），不受影响。**建议阶段 2 先落 64 KiB 约束（方案 A），方案 B 留作设计债记录**。

### 3.3 推荐挂点（Q6 结论，伪代码级）

主挂点 = **Pingora 内建尝试循环**（不自建循环——`proxy_to_upstream` 是私有的，hook 之外没有合法的重入点；也不用子请求机制——它面向内部缓存再验证，构造不出携带任意 body 的子会话）。

```text
request_filter          （不变：路由、chain、一次性）
loop: 由 Pingora 驱动，每轮 = 一次 attempt，上限 max_retries（提为链展开上界）
  upstream_peer(ctx):
    plan = ctx.attempt_chain.current()          # 熔断 open → 跳过记 skipped
    if plan.is_none() or session.is_process_shutting_down(): return Err(不重试)
    return upstream::peer(plan.destination, budget)   # 失败时 Err 上带 retry 位
  upstream_request_filter(req, ctx):            # 每轮一次，本轮写头之前
    llm::apply_upstream_request(req, ctx.attempt_plan)  # path/headers 按 attempt
    ctx.begin_body_of_attempt()                 # attempt ≥2 重置 body 累积（防回放双计）
    framing / x-forwarded-* （现状不变）
  request_body_filter(body, eos, ctx):          # 每轮 0..n 次
    if converting and ctx.prepared:             # 重试轮短路：不二次 prepare
        *body = ctx.encoding_for_attempt()；吞掉后续 chunk
    else: 现状（攒 chunk；eos 时 prepare 并记录 per-attempt 编码）
  upstream_response_filter(resp, ctx):          # 每轮一次，客户端零字节时点
    if resp 2xx: install_transform（现状）；循环结束
    else: classify(attempt) ->
        Serve    => 翻译错误头（现状）
        Attempt  => return Err(retry=true)      # 先于任何下游写（PP/proxy_h1.rs:417→436）
        Exhausted=> 翻译错误头（现状）
  fail_to_connect(e, ctx):                      # 新增 hook
    if ctx.has_next_attempt() and 预算未尽: e.set_retry(true)
  error_while_proxy(e, ctx):                    # 重试决策单行门
    if ctx.has_next_attempt() and session.response_written().is_none()
       and not is_downstream_gone(e): e.set_retry(true) else e.set_retry(false)
  fail_to_proxy / logging                       （现状不变；exhausted 的 canonical 错误在此应答）
```

---

## 4. 探针代码

未使用。控制流的全部断言（循环重入、buffer 回放条件、截断空档、零字节窗口、shutdown 检查点）都有直接的源码行号证据（§2），静态证据已闭合；若第 5 步实装时想验证 64 KiB 边界，e2e（implement.md 第 7 步 `llm_fallback.rs`）加一条 65 KiB 请求的"不重试"用例即可覆盖，无需独立探针。

## 5. 附：证据文件清单

- PP/lib.rs：1148（process_request）、1275-1316（尝试循环）、401-485（proxy_to_upstream）、410-413（upstream_peer Err 短路）、415（get_http_session）、471-483（error_while_proxy / fail_to_connect 调用点）、1347（fail_to_proxy）、572-610（Session 字段）、306-320（await_shutdown）、332-339（新请求 select）、1582-1585（keepalive 关闭）、1598-1602（http_cleanup）、968-970（is_process_shutting_down）、194/230（max_retries 装配）
- PP/proxy_h1.rs：48（req 克隆）、81-90/122（filter→写头）、145（enable_retry_buffering）、214-227（connected_to_upstream）、315-321（读错误→Failed task）、397-436（filter 先于下游写）、483-499（buffer 回放）、1020-1072（send_body_to_pipe→request_body_filter）
- PP/proxy_h2.rs：133（req 克隆）、173-181（filter）、297（buffering）、528-541（回放）
- PP/proxy_trait.rs：585-603（error_while_proxy 默认策略）、605-621（fail_to_connect 文档）、631-669（fail_to_proxy 默认）
- PP/proxy_common.rs：413-478（DownstreamStateMachine，can_poll 433-435）
- PC/protocols/http/v1/server.rs：586-596（buffer 累积）、1118-1123（buffer 清空）、1155-1175（truncated/get_retry_buffer）、1217-1265（read_body_or_idle pending）、830-837（二次写头丢弃）、1579-1587（Failed flush）、655-678/681-683（write_response_header/response_written）
- PC/protocols/http/v1/common.rs:32 与 PC/protocols/http/v2/server.rs:49（BODY_BUF_LIMIT=64 KiB）
- PC/protocols/http/body_buffer.rs:23-33（截断丢内容）
- PC/protocols/http/server.rs:620-636（错误应答对已写响应无操作）
- PC/connectors/http/mod.rs:66-133、PC/connectors/http/v1.rs:35-42（池/新建）
- PC/connectors/l4.rs:158-217、329-332（connect 错误无 retry 置位）
- PC/server/configuration/mod.rs:35、134-137（DEFAULT_MAX_RETRIES=16 及语义）
- PC/server/mod.rs:800-824（grace period 与强制关闭）
- PE/lib.rs:33-44、59-87、200-225（Error.retry 默认 Decided(false)、RetryType 语义）
- nico：service.rs:33-45、proxy/hooks.rs:126-135、proxy/hooks/request.rs:171-257、proxy/hooks/body.rs:40-103、proxy/hooks/response.rs:133-213、proxy/hooks/llm.rs:98-161、proxy/forward.rs:128-160、context/llm.rs:84-87、upstream.rs:47
