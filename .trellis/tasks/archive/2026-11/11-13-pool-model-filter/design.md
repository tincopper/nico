# 设计：provider 池的请求期模型过滤

## 1. 不变量与失败模式

| 不变量 | 违反时的失败模式 |
|---|---|
| **I1 模型是请求期的值，选择是请求前的动作。** 数据面在客户端 body 到达前就选定目标，所以"按 body 里的模型挑成员"不可实现。 | 若假装能实现，就会出现"策略挡住了成员但请求已经发给它"的假过滤，或者无限重选（实测撞上 128 次熔丝）。 |
| **I1b 成员对模型的看法必须一致。** 一个池里的成员要么都声明相同的 `model_allowlist`，要么都不声明。 | 成员各服务不同模型集时，"请求该发给谁"在选目标时无法回答：首轮可能选中一个必须拒绝的成员，而另一个本可服务——调用方拿到 403，网关却知道自己能服务。 |
| **I2 物理模型按成员解析。** 每个成员用自己的 `model_map` 把逻辑名解析成物理名，再查自己的 `model_allowlist`。 | 池里各成员的映射不同时，用错名字过滤会把合法成员挡掉，或把不合法的放进来。 |
| **I3 "不能服务"与"不可用"是两件事。** 模型不匹配由成员的 allowlist 检查回答（`403 model_not_allowed`）；池耗尽仍走既有的 `primary_failure()`。 | 一次普通的池耗尽被误报成调用方的模型错误，运营按错误的方向排查。 |
| **I5 过滤只读物化结果。** 不新增字段、不做 I/O、不重新解析文档。 | 请求期做配置解析会把发布期错误变成运行时错误，且每次请求重复付代价。 |

## 2. 为什么"发送前按 body 过滤"不可实现

Pingora 的调用顺序是 `upstream_peer`（选目标、建 peer）→ `upstream_request_filter`（写 path 与
headers）→ `request_body_filter`（读 body）。模型名只在最后一步之后才能解出来，而那时：

- 目标已选定，peer 已建；
- `path` 与 `headers` 已按该目标写好；
- body hook 的错误走 `fail_to_proxy`，**不会**重跑 `upstream_peer`。

三种实现都在这一点上失败：在 `walk_order` 过滤（首轮没有模型可过滤）、在 `prepare` 里换成员
（path/headers 已写好）、让 `prepare` 返回 `AttemptRetry`（body hook 的错误不触发重跑，实现在
测试里循环到熔丝上限 128 次）。

因此本设计**不承诺**"发送前过滤"，改为两条可实现的约束：

1. **发布期**：一个池的成员必须对模型集合达成一致（I1b），否则整份文档被拒、保 LKG；
2. **请求期**：成员的 allowlist 检查留在编码阶段（既有行为），模型不匹配时 provider 收到请求头
   但**永远收不到 body**，调用方得到 `403 model_not_allowed`。

`walk_order` 的模型过滤仍然保留：它只在**重试轮次**（模型已知）时生效，让一次失败后的下一轮
不会再挑到同一个不匹配的成员。

## 3. 过滤发生在哪

两处，职责不同：

**发布期**（`nico-server/src/llm/plan/chain.rs::ensure_uniform_allowlist`）：一个池的成员编译完成
后，比较各自的 `model_allowlist`；不一致则整份文档被拒（`LlmBuildError::PoolAllowlistMismatch`）。
这把 I1b 变成发布期就能发现的配置错误，而不是请求期的随机 403。

**请求期**（`AttemptSequence::walk_order`）：`decode` 之后逻辑模型已知，池候选按
"该成员能否服务这个模型"过滤，再交给策略。首轮（body 未到）不过滤——那是 I1 决定的；重试轮次
（模型已知）过滤生效，保证一次失败后的下一轮不会再挑到同一个不匹配的成员。

```
upstream_peer ──► select_first ──► walk_order（模型未知，不过滤）
                                        │
request_body_filter ──► decode ──► requested_model 已知
                                        │
                             (重试轮次) walk_order（过滤生效）
                                        │
                             encode_current ──► route.allows() ──► 403 model_not_allowed
```

成员自己的 `allows` 检查在编码阶段，位置没有变。

## 4. 错误通道

`select_from` 已经返回 `SelectionRefused(Box<CanonicalError>)`，`attempt_adapter::select`
把它转成 `HttpError`。本设计复用这条通道：

```rust
// nico-llm/src/route/attempt/pool.rs
fn model_refusal() -> CanonicalError {
    CanonicalError::from_static_parts(
        ErrorCategory::Client,
        "model_not_allowed",
        "the route does not serve this model",
        403,
    )
}
```

与 `ExchangeError::ModelNotAllowed` 的渲染（`filter/error.rs:151`）逐字一致，因此客户端看到的
code / status / category 不因"发生在选择期还是编码期"而不同。

## 5. 池耗尽与模型不匹配

两个不同的"没有可用成员"，回答不同：

| 情形 | 判定处 | 回答 |
|---|---|---|
| 成员的 allowlist 不认这个模型 | `encode_current` → `ExchangeError::ModelNotAllowed` | `403 model_not_allowed`（`ErrorCategory::Client`） |
| 池里所有成员都试过/被排除/熔断打开 | `select_from` → `primary_failure()` | 既有路径，可进 fallback |

`walk_order` 的模型过滤只会让候选集更小，不会改变这个分类：它不产生新的错误类型。

## 6. 与既有约束的组合顺序

过滤、已试、排除、熔断是四条独立的候选约束，按以下顺序应用（顺序本身不影响结果，但影响
诊断可读性）：

1. **模型可服务**（本设计，I1/I2）
2. 本次请求未试过
3. 排除表未退休
4. `select_from` 里的 breaker `admit`

`gateway_llm_pool_selected_total` 只在第 4 步通过时发射，所以被模型过滤掉的成员天然不
产生样本（R4）。

## 7. 备选方案与取舍

| 备选 | 为什么不做 |
|---|---|
| **在 `PoolOrder::order` 里过滤** | 池顺序是通用选择原语，不该知道 provider 的模型语义；且 `PoolOrder` 没有 `Exchange` 可查。 |
| **在 `compile_chain` 发布期按模型拆分池** | 模型是请求期的值，发布期无法枚举；且会为每个模型复制一份 chain。 |
| **让 `prepare` 返回 `AttemptRetry` 重选** | body hook 的错误不重跑 `upstream_peer`：实测同一成员被反复选中，128 轮后撞上熔丝上限。 |
| **在 `prepare` 里就地换成员** | `path` 与 `headers` 已按旧成员写好，换了只会把 body 发给错的地址。 |
| **把所有不可服务成员当失败计入熔断** | 混淆 I3：模型不匹配是路由配置与调用方的 mismatch，不是 provider 故障，不该打开熔断。 |
| **不校验成员 allowlist 的一致性** | 那正是首轮可能选中"必须拒绝"的成员、而另一个本可服务的原因；发布期拒绝是唯一能在发送前发现它的地方。 |
| **新增 `llm.model_filtered` 日志字段** | 现有 `403 model_not_allowed` 的错误记录已足够；一个只在异常路径出现的字段收益低（R4）。 |

## 8. 兼容性与回滚

- 文档兼容：不新增字段。`model_allowlist` 为空时 `allows` 恒真，既有文档行为不变。
- **一处收紧**：一个池的成员若声明了不同的 `model_allowlist`，文档现在被拒。此前它们能发布，
  但请求会随机落到一个不认该模型的成员上——那不是可用行为，所以拒绝对部署方是改进。
- 单成员池与所有成员 allowlist 相同的池：行为与改动前一致。
- 回滚 = 删掉 `ensure_uniform_allowlist`、`walk_order` 的模型过滤与 `model_refusal`，无数据迁移。

## 9. 测试策略

| 层 | 内容 |
|---|---|
| 单测（`nico-llm/tests/route/model_filter.rs`） | 只有能服务的成员被选中；重试轮次不再回到不匹配的成员；全池不服务时 `refuse_unservable_model` 返回 403；无 allowlist 的成员照常服务；按成员自己的 `model_map` 解析物理名 |
| e2e（`nico-server/tests/llm_pool/model_filter.rs`） | 成员 allowlist 不一致的池在发布期被拒、旧 generation 继续服务；一致时正常分发；不认的模型得到 403 且 provider 收不到 body |
| 回归 | 单成员池既有 e2e、全树测试、clippy、fmt、文件 ≤500 行 |
