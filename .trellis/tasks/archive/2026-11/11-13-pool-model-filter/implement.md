# 实施：provider 池的请求期模型过滤

## 交付

- [x] `AttemptSequence` 记住 `requested_model`（`decode` 时读自 canonical 请求）。
- [x] `walk_order` 按模型过滤池候选，并在过滤状态变化时重画顺序（首轮无模型 → 重试轮有模型）。
- [x] `AttemptSequence::refuse_unservable_model`：编码前判定"全池都不服务该模型"，直接 403。
- [x] `AttemptSequence::model_refusal`：`403 model_not_allowed`，与 `ExchangeError::ModelNotAllowed`
      的渲染逐字一致。
- [x] `ProviderRoute::allowlist()` 访问器，供发布期比较。
- [x] `nico-server::plan::chain::ensure_uniform_allowlist`：池成员 allowlist 不一致 → 拒绝整份文档。
- [x] `LlmBuildError::PoolAllowlistMismatch`（`ErrorCategory::Client`）。
- [x] e2e fixture：`provider_with_allowlist` + `model_allowlist` 渲染 + `chat_with_model`。

## 验证

| 项 | 结果 |
|---|---|
| `cargo test -p nico-llm --test route` | 73 passed（含 `model_filter` 6 项） |
| `cargo test -p nico-server --test llm_pool` | 12 passed（含 `model_filter` 3 项） |
| 全树 / clippy / fmt | 见提交记录 |

## 与初始设计的偏离（必须在 PRD/design 里读到）

原计划"选定成员之前按模型过滤候选"。实现时发现 **Pingora 的 hook 顺序使它在首轮不可实现**：
`upstream_peer` 选目标早于 `request_body_filter` 读 body，且 body hook 的错误不重跑 `upstream_peer`。
三次尝试都失败（详见 design §2），其中"返回 `AttemptRetry`"实测循环到熔丝上限 128 次。

最终形态是两条可实现的约束：

1. **发布期**：池成员的 `model_allowlist` 必须一致，否则拒绝文档；
2. **请求期**：成员内的 allowlist 检查保持原样；不认的模型 403，provider 收不到 body；
   重试轮次按模型过滤候选。

这是本次唯一一处**收紧既有行为**：此前成员 allowlist 不一致的池可以发布（但行为不可用）。
