# 实施：LLM token 限额

## 第 1 步：schema

- [ ] `nico-core/src/config/route.rs`：`TokenBudget { limit_tokens: u64, window_seconds: u32 }` +
      `Route::token_budget: Option<TokenBudget>` + 访问器。
- [ ] `raw.rs`：`RawTokenBudget`（两字段必填，`deny_unknown_fields`）。
- [ ] `reference.rs`：`check_target_fields` 拒绝 upstream 路由上的 `token_budget`；resolve 接线。
- [ ] `semantic.rs`：`limit_tokens 1..=1_000_000_000`；`window_seconds 1..=31_536_000`。
- [ ] 单测：读写、upstream 拒绝、范围三种。

**检查点 A**：`cargo test -p nico-core --lib` 绿。

## 第 2 步：store

- [ ] `nico-http/src/quota.rs`：`BudgetPolicy`、`BudgetStore` trait、`InProcessBudgetStore`
      （LRU 有界、固定窗口）、`Exceeded { limit, window_seconds }`。
- [ ] `HttpError::TokenBudgetExceeded` → 429 / `token_budget_exceeded` / Client。
- [ ] `HttpStateHandle.budgets: Arc<dyn BudgetStore>`，`new()` 默认进程内实现。
- [ ] store 单测：开窗、累计、过期重置、LRU、check/charge 共窗。

**检查点 B**：`cargo test -p nico-http --lib quota` 绿。

## 第 3 步：检查与记账

- [ ] `upstream_peer` 开头：route 有 budget 且身份存在 → `check`，超限 `abort` 429。
- [ ] `record_tokens` 尾部：2xx + usage + 身份 → `charge(input + output)`。
- [ ] 数据面单测：有身份超限 429、无身份放行、未超限放行；记账三条件。

**检查点 C**：`cargo test -p nico-http --lib` 绿。

## 第 4 步：e2e 与文档

- [ ] `llm_pool` 新场景：api_key 插件 + `token_budget` 小预算 → 第一次 200、第二次 429；
      provider 只见到一次请求。
- [ ] README：`token_budget` 配置样例与"事后语义 / 无身份不限"两条边界。
- [ ] spec `logging-guidelines.md` 的指标表与 `error-handling.md` 的错误码表补
      `token_budget_exceeded`。
- [ ] roadmap §3 的 token 限额条标注为已交付（外部 KV 为后续任务）。
- [ ] 全树回归、clippy、fmt、traffic 角色、文件 ≤500 行。

## 验证命令

```bash
cargo test -p nico-core --lib
cargo test -p nico-http --lib
cargo test -p nico-server --test llm_pool
cargo test --workspace
cargo clippy --workspace --all-targets
cargo check -p nico-server --no-default-features --features traffic
```

## 落地记录

| 验收面 | 证据 |
|---|---|
| schema 读写与校验 | `cargo test -p nico-core --lib`（`reference::tests::pool` 5 项新增） |
| store 语义 | `cargo test -p nico-http --lib quota`（8 项：开窗/累计/过期/LRU/隔离/跨路由） |
| 检查与记账 | e2e `llm_pool::budget`（4 项：200→429、provider 只见一次、无身份不限、预算内放行） |
| `token_budget_exceeded` 429 | `crates/nico-http/src/error.rs` + e2e 断言 |
| 全树 / clippy / fmt / traffic 角色 / 行数 | 全绿；`semantic.rs` 487 行（budget 校验拆至 `semantic/budget.rs`） |

**多副本边界**：进程内 store 是跨副本外部 KV 的替身。多副本部署在外部 KV 落地前不得声明
依赖共享预算的配置——`BudgetStore` 接口即为此预留，换实现不改检查与记账的调用点。
