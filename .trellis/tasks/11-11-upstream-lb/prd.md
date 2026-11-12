# 上游负载均衡：多实例候选、选择策略与失败剔除

## Goal

把 `upstreams[]` 从一个 `host:port` 扩展成一组 endpoint：按策略选一个，失败时把它剔出候选，
冷却后让回来。今天每条路由只能指向一个地址——`fallback` 是**跨 provider** 的链、`retry_budget` 是
**同一目标**的重试，两者都不是"同一 upstream 的多个实例"。

**本文只覆盖流量层**（同一 upstream 的多个地址）。模型层——一个逻辑模型对应多家 provider、按权重
分发并按失败类别处置——是另一个交付物，见 `11-11-model-lb`；两层共享策略词汇，但候选类型、所在的
crate 与验收都不同。

## Background

- 现状：`Upstream { scheme, host, port }`（`crates/nico-core/src/config/upstream.rs:28-33`），
  `deny_unknown_fields`（`crates/nico-core/src/config/raw.rs:17`）——写 `nodes:` / `weight:` 会直接
  被拒绝，所以这是一次 schema 扩展，不是补一个字段。
- 选择点只有一个：`Destination` → `HttpPeer`（`crates/nico-http/src/upstream.rs`）。`peer()` 今天
  接收的是一个已经确定的 host/port。
- pingora 的 `lb` feature 未启用（`crates/nico-http/Cargo.toml` 只有 `proxy` + `rustls`）；
  `pingora-load-balancing` 带着它自己的 `Backend` / 健康检查模型。
- 上游 TLS 已在 `11-11-upstream-tls` 关闭：connector 在 service 构建时定型、信任根是进程级事实，
  **LB 不该碰 TLS 配置**，但必须保住 SNI / `Host` 的取值来源（见 R5）。
- 失败语义已经有两层：`retry_budget`（同目标重试，含 64 KiB 重放上限）与 `fallback`（跨 provider，
  发布期物化、能力取交集）。LB 的剔除是第三层，必须与它们正交。

## Requirements

### R1 配置：一组 endpoint，且单实例写法仍然有效

- `upstreams[]` 的 `scheme` + `host` + `port` 保持现状，但它们从此只描述**逻辑身份**：`host` 是写进
  `Host` 头与 TLS SNI 的名字，`scheme` 与 `port` 是该 upstream 的默认值。
- 可选 `endpoints: [{ name, address?, port?, weight? }, …]`：`address` 是**连接目标**（IP 或主机名），
  缺省 = upstream 的 `host`；`port` 缺省 = upstream 的 `port`。endpoint 不携带 `scheme`：一个 upstream
  只有一种传输方式。
- 两者同时存在时，`scheme`/`host`/`port` 仍是必填的（它们提供逻辑身份与默认值）；**不写 `endpoints`
  等于一个成员**，数据面因此只有一条选择路径——不保留"单实例特例"分支。
- `endpoints` 非空、不重复；上限由 `schema_bounds` 新增一个有界字段约束（与 routes/upstreams 同样的
  做法）。
- `name` 在所属 upstream 内唯一；它是日志与指标里的唯一标识（不是 `address:port`，见 R6）。
- `weight` 可选，默认 1，范围 `1..=1000`。`weight: 0` 在流量层没有"只兜底"的含义（流量层没有
  fallback），所以流量层不接受 0。

### R2 选择策略

upstream 级 `strategy`，四选一，默认 `round_robin`：

| 值 | 语义 |
|---|---|
| `round_robin` | 顺序轮转（默认） |
| `weighted_round_robin` | 按 `weight` 比例的平滑加权轮转（不是随机） |
| `least_conn` | 当前活跃连接数最少者，平票按轮转 |
| `ip_hash` | 客户端地址哈希，同一调用方落在同一实例 |

`least_conn` 需要活跃连接计数：Pingora 的连接池是进程级的，池内连接不属于某个请求，所以计数必须
由数据面在"选中 → 请求结束"之间自己维护（见 design 的取舍）。`ip_hash` 用客户端地址，**不**用
`Host` 或路径——否则同一网吧的调用方会被打散到所有实例。

### R3 失败剔除与恢复（被动，不做主动探测）

- 每个 endpoint 一个失败计数：连续失败达 `max_fails`（默认 3）→ 剔除，进入冷却
  `fail_timeout_ms`（默认 30000）。
- 冷却结束后不是立刻恢复满额，而是放行一个试探请求：成功则清零并完整回归，失败则重新冷却。
- "失败"只包括**连接/握手阶段的失败**与**响应已提交前的 5xx**；4xx（调用方的问题）不计数，
  否则一个坏客户端能把整组实例踢空。
- 剔除状态是**进程级运行态**，与 generation 无关：配置改了这个 upstream 的 endpoint 集合，只有
  身份（upstream 名 + endpoint 名 + 地址）相同的成员继承历史，其余归零——与熔断器跨 generation
  的继承规则同源。
- **全部被剔时直接拒绝**：这一组没有可用实例时返回 `502 upstream_unreachable`，不从已剔除的
  成员里再挑一个。整组同时被剔说明网关自己看到的失败是普遍的，此时把请求发给一个刚被判定失败的
  地址只会把延迟叠在失败上；拒绝让故障显式可见。

### R4 与 `retry_budget` / `fallback` 正交

- 剔除发生在**选择**这一层：被剔除的成员不参与选择，但不改变"重试同一目标"与"换一个 provider"
  的既有判定。
- 一次请求选定 endpoint 后，本次请求的重试仍在**同一个 endpoint** 上（`retry_budget` 语义不变）；
  只有 `fallback` 换 provider 时才重新选择 endpoint。
- 决策点仍只有一处：`proxy::hooks::llm::retry_admitted` 与 `HttpContext::may_replace_attempt`
  不动。

### R5 SNI / `Host` 的取值来源不变（TLS 接缝）

- `Host` 头与 TLS SNI 取 `upstreams[].host`（逻辑身份），**不取被选中 endpoint 的 `address`**。
  因此"同一 vhost 背后多台机器"和"同 host 多端口"都能通过证书校验：证书签的是逻辑 host。
- endpoint 的 `address` 若不是 IP 而是另一个名字，它只参与解析与连接，仍不参与 SNI 与证书校验。
  一条 upstream 的证书必须覆盖 `host`——这是正确的约束，不是需要放宽的地方。

### R6 可观测性

- 指标：`gateway_upstream_selected_total{upstream, endpoint}` 与
  `gateway_upstream_ejected_total{upstream, endpoint}`（标签取配置里的名字，基数可控）。
- 日志：请求记录的 `upstream` 字段保持 upstream 名，新增 `upstream.endpoint` 只在该 upstream 有
  多个成员时写入（单实例部署的日志形状不变）。
- 剔除与恢复各一条 `warn!` / `info!`，字段 `upstream`、`upstream.endpoint`、原因。

### R7 依赖与实现边界

- **不启用 pingora 的 `lb` feature**：它的 `Backend` 与健康检查模型会和"发布期物化 + 运行态继承"
  的既有约束打架，且它自带一套选择状态要先学会。此条是设计决策，需要在 `design.md` 里论证
  （备选：`pingora-load-balancing`）。
- 选择器住在 `nico-http`（它已经拥有"如何抵达 peer"），配置模型住在 `nico-core`。
- 不引入新 crate；不碰 TLS 的进程级配置。

## Out of Scope

- 主动健康检查（周期探测、独立探测路径、独立超时）。
- DNS 多 A 记录自动展开成候选集。
- 一致性哈希、区域感知、按请求体内容选择目标。
- **会话粘性 / session affinity**（bifrost 有，nico 无会话概念）；它需要先有可共享的 KVStore。
- 按 token 用量或剩余配额做分配（见 R10，依赖 usage 归一化）。
- 上游 HTTP/2、mTLS、每 provider 一份 CA（与 TLS 任务的边界一致）。
- 连接池参数调优（`keepalive` 池大小、每 endpoint 独立池）。

## Acceptance Criteria

- [ ] 单实例写法的既有文档与测试**零改动**通过；`endpoints` 与单实例字段同时出现被拒绝且字段名正确。
- [ ] 四种策略各有单测：`round_robin` 的轮转序列、`weighted_round_robin` 的比例与平滑性、
      `least_conn` 在计数变化下的选择、`ip_hash` 对同一地址的稳定性与不同地址的分散。
- [ ] 剔除：连续失败达阈值后该 endpoint 不再被选中；冷却后只放一个试探；成功清零、失败重新冷却；
      4xx 不计数；全部剔除时退化为"最久未尝试"并记录 `warn`。
- [ ] 跨 generation：endpoint 集合不变则历史继承，改集合则只有身份相同的成员继承。
- [ ] e2e：一个 upstream 两个实例，杀掉一个 → 流量全部落到另一个（有断言，不靠 sleep），
      再让它回来 → 冷却后重新参与。
- [ ] SNI/Host：多实例同 host 不同端口时，TLS 上游仍通过证书校验（复用 `11-11-upstream-tls` 的
      fixture）；`Host` 头与 SNI 都是 upstream 的 host。
- [ ] 指标与日志字段按 R6 出现，标签基数不随请求增长。
- [ ] 五条基线全绿、角色矩阵两个组合都能编译、全树 ≤500 行、`cargo tree -d` 无新增重复栈。


## Open Questions

（已定：默认 `round_robin`、全组被剔直接 502、`max_fails = 3` / `fail_timeout_ms = 30000`、
`ip_hash` 用客户端地址。）

（无其它待决项。模型层不在本任务，见 `11-11-model-lb`。）
