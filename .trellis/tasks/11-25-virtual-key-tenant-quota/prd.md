# 虚拟 key 与租户 token 预算

## Goal

让多个虚拟 API key 映射到同一 tenant 时，在同一 convert route 内共用 provider-reported token 预算，同时保留每个 key 自己的 principal 身份用于认证归因与日志。未配置 tenant 的旧 API-key、JWT 与其他认证插件维持现有 subject-scoped 行为。

## Confirmed Facts

- `Identity` 当前保存 `subject` 和认证插件 `source`；`HttpContext::set_identity` 与 `PluginContext::set_identity(subject)` 均为 first-writer-wins。
- API-key config 当前以每个 entry 的 `name` 作为认证 subject，并在 publication 阶段 materialize `secret_ref`；key 比较为 constant-time。
- Token budget 当前按 `(route_id, identity.subject())` 产生固定宽度 key；在 upstream dispatch 前检查，在成功响应后依据 provider reported `input + output` 记账。
- 预算为固定窗口、bounded in-process LRU；请求可能使额度超限，后续请求收到 `429 token_budget_exceeded`。无认证 identity 的请求目前不检查也不记账。
- WASM 通过同一个 PluginContext contract 读取 request facts；当前事实包括 authenticated subject，不含 tenant。

## Requirements

### R1 | Virtual key to tenant mapping

- API-key entry 可选声明 `tenant`，作为 quota grouping identifier。
- 每个 key 的 `name` 仍是 principal/subject，不被 tenant 替换；`source` 仍是认证 plugin instance。
- 同一 API-key instance 下可以有多个不同 key name 映射到同一 tenant；相同 key name 仍拒绝。
- 未声明 tenant 的 entry 产生没有 tenant 的 identity，并继续按现有 subject 维度计量。
- Tenant identifier 使用与现有 key name 相同的 bounded, printable ASCII label 规则；配置拒绝空值、超界值、不可打印字符以及 tenant 形式的控制字符。
- Presented key、materialized secret 和 secret reference 不得进入 tenant identity、错误正文或日志；secret 继续只在 publication materialization 阶段读取。

### R2 | Atomic authenticated identity

- PluginContext 增加兼容性扩展，可由认证插件一次性建立 `{subject, tenant?}`；不能通过先写 subject、再写 tenant 的两个独立操作留下半完成身份。
- 现有 `set_identity(subject)` 保持语义：写入 tenant 为 `None` 的身份，JWT 和未迁移的认证插件不改变行为。
- First-writer-wins 对整个 identity tuple 生效：后续认证插件不能替换 subject、tenant 或 source。
- Only `Authenticate` request phases may write identity; the existing read-only identity facts remain available in later phases for policy checks, accounting and logging.

### R3 | Per-route tenant-shared token budget

- 存在 tenant 的 identity 使用 `(route_id, tenant)` 作为 token-budget grouping key；tenant 内多个虚拟 key 的 provider-reported usage 汇总到同一个 route window。
- 不存在 tenant 时使用 `(route_id, subject)`，保持现有 JWT、旧 API-key 和其他 plugin 的额度隔离行为。
- `source` 不加入 quota key；同一 tenant 的不同 key/plugin source 在同一路由共用额度。
- 不同 route 仍各自拥有独立预算；跨 route、跨 provider/member、request-count `rate_limit` 均不变。
- 检查、记账时点、固定窗口、额度配置与 429 response contract 保持现状。一次成功请求允许事后跨过额度，下一次请求被拒。
- Storage 仍是现有 bounded process-local BudgetStore；不承诺多个进程/副本间共享。

### R4 | WASM tenant visibility

- WASM `on_request` facts 增加可选 tenant；未设置时序列化为 `null`，不改变既有 subject/config facts。
- WASM 不能设置 principal/tenant，不能读取 secret 或 provider credential；本任务不新增 WASM authentication action、phase 或宿主权限。

### R5 | Compatibility and exclusions

- 不增加全局 tenant registry；tenant 归属由 API-key entries 静态声明，配置随 generation publication 原子替换。
- 不做 tenant hierarchy、key-level secondary quota、跨 route shared pool、按 provider/kind 分桶、provider credential 按 tenant 切换或 key issuance/revocation API。
- 不实现外部 KV/跨副本原子配额。部署多副本时额度仍各副本独立；不得宣传为集群级全局预算。
- `config-nacos` 不属于本任务。

## Acceptance Criteria

- [ ] API-key schema 接受可选 tenant；缺失时保持旧配置解析；空、过长、非 printable ASCII tenant 拒绝且错误只点名字段。
- [ ] publication 按现有 bounded secret materializer 读取 key；tenant 不影响 constant-time compare，错误和 log 不包含 secret。
- [ ] 虚拟 key 成功认证后 identity 保留 key name 为 subject、API-key instance 为 source，并同时携带 tenant。
- [ ] 两个不同 key subject 映射同 tenant，在同一路由共享 token budget；到额度后的下一个请求得到 `429 token_budget_exceeded`，provider 不收到该请求。
- [ ] 同 tenant 在不同 route 的预算隔离；无 tenant identity 继续按 subject 隔离；同 subject 多 source 的现有行为保持不变。
- [ ] first-writer-wins 针对 `{subject, tenant, source}` 整体生效；非 Authenticate phase 不得写 tenant identity。
- [ ] 检查仍在 dispatch 前；只对成功且有 reported usage 的响应计 `input + output`；非 2xx、缺失 usage 或缺失 identity 不记账。
- [ ] WASM request facts 可读取 tenant；无 tenant 时为 `null`；WASM 没有 identity/tenant 写入能力。
- [ ] tenant-only delta 不改变既有 JWT/无 tenant API-key 请求、request-count rate limit、provider credential 路径或 reload 的 LKG 行为。
- [ ] 全量 backend gates、feature matrix、dependency boundaries 与 500-line budget 通过。

## Out of Scope

Cross-replica quota consistency and external storage; cross-route/tenant hierarchy budgets; provider credential routing; request-count quotas; API management lifecycle for creating/revoking virtual keys; `config-nacos`.
