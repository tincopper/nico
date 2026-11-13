# 补齐 provider 池的请求期模型过滤

## Goal

落地 `11-11-model-lb` 的 R5：一条路由的池成员各自可能服务不同的模型集合，请求进来时应先按**本次
请求解析出的物理模型**过滤候选，再把过滤后的集合交给策略选择。全部成员都不能服务该模型时，回答
`403 model_not_allowed`，不进 `fallback`。

## Background

- PRD R5 原文：`model_map` / `model_allowlist` 已在发布期物化，请求期是查表；过滤后候选为空
  → 不进 `fallback`，直接 `403 model_not_allowed`（"路由不服务该模型"而不是"成员都挂了"）。
- 现状：`ProviderRoute::allows(physical)`（`provider/route.rs:225`）与
  `ProviderRoute::model(logical) -> ModelRef`（`:241`）已存在，但只在**编码时**由
  `ExchangeError::ModelNotAllowed` 检查。`route/attempt/pool.rs::walk_order` 构造候选时不看模型。
- 后果：一个成员必然拒绝该模型的请求仍会被发过去，然后 403 被当成失败，浪费一次尝试并可能
  触发无意义的成员切换。
- `SelectionRefused`（`attempt.rs`）携带一个 `CanonicalError`，`attempt_adapter::select` 把它
  转成 `HttpError`——这正是"全部不可服务"要用的通道。
- `CanonicalError::from_static_parts(Client, "model_not_allowed", ..., 403)` 已由
  `ExchangeError::ModelNotAllowed` 使用（`filter/error.rs:151`），可在 crate 内复用。

## Requirements

### R1 池成员的模型集合必须一致

- 一个池的成员要么声明相同的 `model_allowlist`，要么都不声明。不一致时整份文档在发布期被拒、
  保 LKG，错误指出第一个与首成员不一致的 provider。
- 理由：数据面在客户端 body 到达前就选定目标（`upstream_peer` 早于 `request_body_filter`），
  "按 body 里的模型挑成员"不可实现。成员各服务不同模型集时，首轮可能选中一个必须拒绝的成员，
  而另一个本可服务——调用方拿到 403，网关却知道自己能服务。发布期拒绝让这个形状根本进不了生产。

### R2 请求期：成员内的 allowlist 检查（既有行为不变）

- 选定成员后，编码阶段仍由该成员的 `ProviderRoute::allows` 检查；不认的模型回答
  `403 model_not_allowed`（`ErrorCategory::Client`）。provider 会收到请求头，但**永远收不到 body**。
- 重试轮次（模型已知）的候选会按模型过滤，保证一次失败后的下一轮不再挑到同一个不匹配的成员。

### R3 不新增配置面

- 复用已有的 `model_map` / `model_allowlist`，不新增文档字段、不新增 schema 校验。
- 不改 `Provider` trait、不改 `Endpoint`、不改 `PoolOrder` 的签名。

### R4 观测

- `403 model_not_allowed` 走既有的错误渲染与请求记录，不新增日志字段。
- 被模型过滤掉的成员不产生 `gateway_llm_pool_selected_total` 样本（它从未被选中）。

## Acceptance Criteria

- [x] 单测：池中只有部分成员能服务请求模型时，其余成员不被选中。
- [x] 单测：重试轮次不再回到不匹配的成员；全池不服务时返回 `403 model_not_allowed`。
- [x] 单测：无 `model_allowlist` 的成员照常服务；按成员自己的 `model_map` 解析物理名。
- [x] e2e：成员 allowlist 不一致的池在发布期被拒，旧 generation 继续服务。
- [x] e2e：成员 allowlist 一致时池正常分发；不认的模型得到 403 且 provider 收不到 body。
- [x] 单成员池的既有 e2e 零改动通过；全树 ≤500 行、clippy 零告警、五条基线绿。

## Out of Scope

- 按 token 用量 / 按延迟调权、会话粘性（M3，前置未满足）、异构 Provider（已调研并排除）。
- 改 `model_map` / `model_allowlist` 的既有语义。
- 治理层（虚拟 key、租户预算）。

## Open Questions

（无。接缝与错误通道已在 Background 中逐条核实。）
