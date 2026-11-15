# 设计：LLM token 限额

## 1. 不变量与失败模式

| 不变量 | 违反时的失败模式 |
|---|---|
| **I1 检查先于派发。** 预算判定发生在 `upstream_peer` 选目标之前。 | 超限请求被发给 provider 再拒绝，预算形同虚设，且烧掉一次真实调用。 |
| **I2 键是身份，不是地址。** `(route, subject)` 定一个预算。 | 键取客户端地址会让 NAT 后的整个办公室共享一个预算；键取路由会让所有用户互相挤占。 |
| **I3 记账只算全价桶。** `input + output` 计入预算；cache 与 reasoning 不单独计。 | 折扣桶计入会高估消耗，把还没花完的用户挡在外面。 |
| **I4 无身份不放大队。** 身份缺失的请求不受预算约束，这是文档行为。 | 静默用一个共享匿名桶会让所有匿名请求互相挤占，且让"要不要全覆盖"变成实现的隐式决定。 |
| **I5 事后记账是诚实的。** 用量响应后才知；一次请求可越过预算，下一个被拒。 | 假装能"精确到不越过"就需要预估 token 数，而网关没有 tokenizer（usage 调研的结论）。 |
| **I6 表有界。** LRU 驱逐最久未用的预算。 | 键来自身份，无界表让一个换着身份打的客户端涨满进程内存。 |

## 2. 语义

固定窗口：第一次记账开窗；`now ≥ window_start + window_seconds` 时重置。`check` 与 `charge`
共用同一段重置逻辑，所以检查时看到的永远是当前窗口的已用量。

```
upstream_peer
   ├── route.token_budget?            ── 无 → 既有行为
   ├── ctx.identity()?                ── 无 → 不限（I4）
   ├── store.check(key, limit, window, now)
   │     └── 超限 → 429 token_budget_exceeded
   └── select / dispatch
...
record_tokens (2xx + reported usage)
   └── store.charge(key, input + output, now)
```

**事后语义**（I5）：`charge` 发生在响应后，可以把用量推过 limit；那时请求已经服务完毕，
拒绝的是**下一个**请求。这不是缺陷——精确预判需要 tokenizer，网关没有。

**成本归属**：provider 收到了一个 200 的完整调用，费用已经发生；429 拒绝的是"再花一笔"，
不是"不认账"。这是所有 LLM 网关的通行做法。

## 3. 接口

```rust
// nico-http/src/quota.rs
/// One identity's budget for one route, as the route's document states it.
pub(crate) struct BudgetPolicy {
    limit_tokens: u64,
    window_seconds: u32,
}

/// Where token budgets live. The in-process table is the standing implementation; a deployment
/// that runs multiple replicas replaces it with one backed by shared storage, and the check/charge
/// call sites do not change.
pub(crate) trait BudgetStore: Send + Sync + std::fmt::Debug {
    /// Returns whether `key` may spend another request under `policy` at `now`.
    fn check(&self, key: &[u8; 32], policy: &BudgetPolicy, now: Instant) -> Result<(), Exceeded>;
    /// Adds `tokens` to `key`'s current window at `now`.
    fn charge(&self, key: &[u8; 32], tokens: u64, policy: &BudgetPolicy, now: Instant);
}

/// The standing implementation: a bounded LRU of fixed windows.
pub(crate) struct InProcessBudgetStore { /* LruCache<[u8;32], Window> */ }
```

策略（limit/window）随调用传入而不是存在表里，与配置生灭解耦：reload 改了预算，窗口里的已用量
仍然属于那个身份——**换参数不清账**，同 `rate_limit` 的 Limiter 决策。

## 4. 键与表

- 键：`hash(route_id ‖ 0x1f ‖ subject)`，32 字节——不存明文，一次查找零分配（rate_limit 同款）。
- 表：`LruCache<[u8;32], Window>`，`max_keys` 默认 10_000（与 rate_limit 同量级）。
- `Window { used: u64, started: Instant }`。

## 5. 放置

| 件 | 位置 |
|---|---|
| `BudgetPolicy` / `BudgetStore` / `InProcessBudgetStore` | `nico-http/src/quota.rs`（新模块） |
| 进程级持有 | `HttpStateHandle.budgets: Arc<dyn BudgetStore>`，跨 reload 存活（`UpstreamPools` 同款） |
| 配置 | `nico-core` `Route::token_budget` |
| 检查 | `proxy/hooks/request.rs::upstream_peer` 开头 |
| 记账 | `proxy/record.rs::record_tokens` 尾部 |
| 拒绝 | `HttpError::TokenBudgetExceeded` → 429 |

`upstream_peer` 是检查点，因为它在身份已定（auth 插件已跑）、路由已匹配、且任何上游交互之前。

## 6. 错误

```rust
#[error("the token budget for this route is spent: {limit} tokens per {window}s")]
TokenBudgetExceeded { limit: u64, window_seconds: u32 },
```

→ `429`，code `token_budget_exceeded`，`ErrorCategory::Client`。与 provider 的
`upstream_rate_limited` 不同码不同源：一个是网关的策略，一个是 provider 的限流。

## 7. 备选方案与取舍

| 备选 | 为什么不做 |
|---|---|
| **做成插件** | 记账点在 `record_tokens`，插件契约的 `PluginContext` 不暴露 usage 与预算检查所需的状态；为此拓宽插件契约是一个大得多的改动。 |
| **滑动窗口 / 日历对齐** | 固定窗口实现最小且可测；预算语义下窗口边界的少量漂移不改变行为。需要对齐时是后续参数。 |
| **计入全部五种 kind** | 折扣桶计入会高估；v1 只算 input+output，文档写明。 |
| **无身份也限（共享匿名桶）** | I4：让"要不要全覆盖"变成实现的隐式决定。部署方放 auth 插件即得全覆盖。 |
| **预判 token 数再决定发不发** | 需要 tokenizer；usage 调研已排除。 |
| **把 store 放进 `SharedServices`** | 那是 `nico-server` 的 LLM 装配层；检查点在 `nico-http` 的数据面，预算随 `HttpStateHandle` 走才与检查点同侧。 |

## 8. 兼容性与回滚

- 文档新增可选字段；不写的路由行为不变。
- 单副本是当前唯一支持的形态；多副本部署在外部 KV 落地前**不得**声明依赖共享预算的配置。
- 回滚 = 删 schema 字段、`quota.rs`、两处调用点，无数据迁移。

## 9. 测试策略

| 层 | 内容 |
|---|---|
| schema | 字段读写、upstream 路由拒绝、limit/window 范围 |
| store | 首次记账开窗、窗口内累计、过期重置、LRU 有界、check 与 charge 的窗口共享 |
| 数据面 | 有身份超限 → 429；无身份 → 放行；未超限 → 放行 |
| 记账 | 2xx 记 input+output；非 2xx 不记；无身份不记 |
| e2e | api_key 身份 + 小预算：第一次 200、第二次 429；provider 只见到第一次的请求 |
