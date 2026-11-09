# 阶段 2：Provider 与长寿命状态（可靠性核心）

## Goal

阶段 2 里程碑。在阶段 1 之上交付**可靠性核心**：一个 provider 故障不再拖垮路由——响应未提交时
可安全重试、provider 持续故障时熔断、route 声明了兼容链时可换模型——并以 traffic/llm 两种 role
构建支持独立部署。

用户决策（2026-11-08）：范围取 A 安全重试 + B 熔断 + C 模型 fallback + F SharedServices 最小形态
+ G role 构建；D usage 归一化/计费、E LLM token 限额、H 异构 Provider 留待后续里程碑
（D 需要先定计费语义，E 依赖 D，H 设计语义未写清）。

## Background and Confirmed Facts

- 依据：`archive/2026-11/11-03-unified-gateway-analysis/design.md` §6.2/§9/§12 与 `implement.md` §3；
  既有契约见 `.trellis/spec/backend/error-handling.md`（"LLM fallback" 节）与 1.4b `design.md` §10。
- 阶段 1 已成立、本里程碑直接沿用的机制：
  - canonical 可重放：`Exchange::prepare` 产出 `PreparedExchange`（canonical + wire bytes），重试单位已存在。
  - 失败闭表：provider 状态 → `CanonicalError`（含 `retryable` 标记，408/429/5xx = true；401/403/404 = false）。
  - "响应已提交"的判定点已存在：response head 在翻译之后才写给客户端；流式答复发头即提交。
  - 能力门：请求需求 vs 路由能力交集，422 `unsupported_capability`。
  - 长寿命状态先例：插件限流表按 quota 未变即跨 generation 复用；LLM 凭证发布期物化一次。
- 代码现状：无 retry/熔断/fallback 实现（"one attempt: this build never retries"）；无 `SharedServices`；
  无 `[features]`、单一 `nico` binary。

## Requirements

### R1 安全重试

- 路由可声明重试预算（次数，含首次；默认 1 = 不重试，上限可配）。仅当 **canonical 错误标记
  retryable** 且 **客户端未收到任何字节**（响应头未提交）时重试；流式答复发头后一律终止，不重试。
- 重试重新编码同一个 canonical 请求（不重放原始 body），重新走目标选择的失败判定。

### R2 熔断

- provider（编译后的 endpoint 身份）级故障预算：连续/窗口失败达到阈值即打开；半开探测恢复。
- 熔断打开时请求直接失败（不尝试该目标），fallback 链跳到下一目标；无下一目标则按 canonical 错误应答。
- 状态跨 generation 复用：目标身份对应的配置未变则继承状态，变了则重置。

### R3 模型 fallback

- 路由可声明**显式** fallback 链（provider 条目序列，操作者声明"兼容"，网关不猜测等价）。
- 发布期编译整条链：每个成员的 endpoint/凭证在发布期物化；链能力 = 全体成员能力交集，
  请求需求超集检查沿用既有能力门。
- 触发条件与重试一致：retryable 失败、响应未提交。SSE 首字节后不换源。
- 计费不重复：usage 按实际服务的目标记录一次；失败目标若已报 usage（罕见）以日志记录、不计入答复。

### R4 长寿命状态落点（SharedServices 最小形态）

- `nico-server` 创建进程级 `SharedServices`（承载熔断/故障计数状态），发布链路与快照以 `Arc` 引用。
- 状态按目标身份继承：配置未变跨 generation 保留；不做通用 Lifecycle Manager。

### R5 traffic/llm role 构建

- Workspace 提供 `traffic` 与 `llm` 两种 feature 组合与对应产物；`llm` 含 `nico-llm`，`traffic` 不含。
- 角色合法性启动时校验：traffic 角色的进程遇到 `routes[].provider` 一律发布失败（不是静默跳过）。
- `--no-default-features` 与各 role 组合均编译通过。

## Out of Scope

- D usage 归一化/计费、E LLM token 限额（依赖计费语义，后续里程碑）。
- H 异构 Provider（语义未澄清；本期 provider 集合 = 阶段 1 的四个 kind 不变）。
- 按请求 body `model` 选上游（"有界缓冲 + 单次重放"独立设计项）。
- etcd / Nacos、metrics exporter、WASM 动态插件、新协议数据面（既有结论不变）。

## Acceptance Criteria

- [ ] 重试：mock 上游第一次 502、第二次 200 → 客户端收到 200 答复，上游恰两次请求，body 逐字节相同
      （canonical 重编码）；非 retryable 失败（401）不重试；发头后失败不重试；预算耗尽按 canonical 错误应答。
- [ ] 熔断：连续失败达阈值后该目标被跳过（上游连接数为 0），半开探测成功后恢复；配置未变的 reload 继承状态，
      改了配置的目标重置。
- [ ] fallback：声明 A→B 链的路由，A 持续 5xx 时请求由 B 服务且答复 200；链能力交集生效（B 缺的能力
      在编译期就让请求 422）；流式答复发头后 A 死亡 → 流终止，不换 B。
- [ ] 重试/fallback 期间客户端 status/body 只反映最终结果；日志记录每次尝试的目标与结果（无 body、无凭证）。
- [ ] role：`llm` 角色进程服务 AI 路由；`traffic` 角色进程遇到 provider 路由发布失败且旧 generation 保活；
      三种 feature 组合 `cargo check` 通过。
- [ ] 五条基线全绿；文件预算 ≤500 行；`nico-core`/`nico-config` 依赖树无越界项。

## Open Questions（阻塞规划）

（无——范围已定，语义由归档设计与既有 spec 固定；重试预算/熔断阈值的默认值是设计层决定，全部可配。）
