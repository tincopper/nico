# 设计：可扩展的跨副本 token budget 存储

## 1. 设计目标与不变量

本任务把“预算状态存在哪里”从请求路径中解耦出来，同时保留现有 token budget 和 tenant quota 的业务语义。

| 不变量 | 违反时的后果 |
|---|---|
| quota key 仍是 `(route_id, tenant)` 或 `(route_id, subject)` | 不同 route 或不同身份错误共享额度 |
| policy 由当前 published route 随请求传入 | reload 后窗口累计值被错误清零或旧策略永久滞留 |
| 窗口由后端可比较的 wall-clock/expiry 表达 | 不同副本的 `tokio::time::Instant` 不能比较，窗口会漂移 |
| check 与 charge 各自是后端原子操作 | 多副本 read-modify-write 丢增量或并发覆盖 |
| 事后 charge 允许 `used` 越过 limit | 网关不会因为事后才知道的 provider 用量而少记真实成本 |
| charge 有幂等身份 | 网络超时重试不会重复扣费 |
| exceeded 与存储故障是不同错误 | 基础设施故障不会伪装成 429 配额超限 |
| 共享模式的故障策略显式配置 | 网关不替部署方决定可用性与配额绕过的取舍 |

“共享”在本版本承诺所有副本对同一窗口执行原子累计，不承诺零超额硬上限。请求 token 在 provider
返回前未知，因此既有 post-hoc 语义保留：并发请求可以同时通过 pre-check，成功后各自 charge，
最终窗口可能超过 limit；后续请求按已累计值拒绝。

## 2. 分层与依赖方向

```text
nico-http
  └── quota domain contract (async trait, policy/key/result/error)
       ↑ implemented by
nico-quota-storage (new domain adapter crate)
  ├── in-process adapter (or nico-http's existing local implementation)
  ├── etcd adapter + quota-specific etcd client seam
  └── Redis adapter + quota-specific Redis client seam
       ↑ assembled by
nico-server
  └── bootstrap quota storage config + state construction
```

`nico-http` 只拥有请求所需的 quota vocabulary，不依赖 etcd、Redis、TLS client 或连接池。具体 adapter
可以放在一个独立的 `nico-quota-storage` crate，避免 `nico-server` 直接承载两个后端的协议细节；该 crate
按 Cargo feature 编译 `etcd` / `redis`，默认不把任一外部客户端带进单副本构建。若实现阶段证明独立
crate 会重复现有 owner 边界，可将 adapter 下沉到 `nico-server` 的 `quota/` 目录，但接口仍留在
`nico-http` 的公开 domain contract，不能让请求路径依赖具体客户端。

现有 `nico-config-etcd` 继续只承担 `ConfigSource` 的 `get/watch`。quota adapter 可以复用它已验证的
TLS、认证、运行时生命周期经验，但必须拥有独立的 transaction/TTL client seam；不能把
`EtcdConfigSource` 或 `EtcdClient` 改成混合接口。Redis 同样通过自己的 seam 进入，不能把 wire client
暴露给 `nico-http`。

## 3. Domain contract

现有同步 `BudgetStore::check` / `charge` 不足以表示远端 I/O 和故障。改为可注入的异步契约，抽象两个
业务时间点但要求每个后端操作内部原子：

```rust
#[async_trait]
pub trait BudgetStore: Send + Sync + Debug {
    async fn check(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        request: &RequestAttempt,
    ) -> Result<CheckResult, BudgetStoreError>;

    async fn charge(
        &self,
        key: &BudgetKey,
        policy: &BudgetPolicy,
        usage: &Charge,
    ) -> Result<(), BudgetStoreError>;
}
```

实际类型名可在实现阶段按现有命名调整，但契约必须满足：

- `BudgetKey` 表达已经完成 domain separation 的固定宽度 key；不把 tenant 放进 metric label，也不把
  credential 放进远端记录；
- `BudgetPolicy` 暴露 limit/window，不暴露配置来源；
- `check` 原子读取、按 expiry 判断是否重置、创建窗口（必要时）、判断 `used >= limit`，返回
  `Allowed` 或带 limit/window 的 `Exceeded`；它不预留未知的 provider token；
- `Charge` 携带本次成功 provider exchange 的稳定 `charge_id`、input/output 合计和请求产生时间的
  domain facts；后端以 `(namespace, key, charge_id)` 去重；
- `charge` 原子读取/重置/去重/累加/刷新 TTL；即使累加后超过 limit 也必须写入；
- `BudgetStoreError` 至少区分 `Unavailable`、`Timeout`、`Conflict`、`Unauthorized`、`Corrupt` 和
  `Invalid`，并能保留日志来源但不把 endpoint、凭证或远端 body 放到客户端响应；
- `CheckResult::Exceeded` 与 `BudgetStoreError` 不可互相转换；请求层才能按配置把后者映射为 fail-open
  或独立的 503。

`RequestAttempt` 的稳定 `charge_id` 必须在一次实际成功 provider exchange 的生命周期内固定，并在
重试/重复 logging hook 中保持相同值。若 Pingora 无法提供天然 request id，则由 `HttpContext` 在请求
开始生成不可预测但不含凭证的 ID；它只用于后端幂等，不进入高基数指标。

本地实现可以保留互斥锁和 `tokio::time::Instant`，但应通过同一 async contract 实现；共享实现使用后端
server time 或持久化 `expires_at`，不跨进程传递 `Instant`。

## 4. 请求与响应数据流

```text
request_filter
  └── HttpContext carries Arc<dyn BudgetStore>
upstream_peer (async)
  └── store.check(key, policy)
       ├── Allowed → select peer and forward
       ├── Exceeded → 429 token_budget_exceeded
       └── StoreError → configured fail-open (continue) or fail-closed (503 quota_store_unavailable)

successful response logging (async propagation)
  └── derive reported input + output and stable charge_id
       └── store.charge(key, policy, charge)
            ├── Ok / duplicate → record normal usage
            └── error → apply configured charge-failure policy, metrics/logging, bounded retry
```

Pre-check errors can still determine the wire response before upstream contact. Charge occurs after the response
has been committed in the existing logging path, so a charge failure cannot rewrite that response. The design must
therefore make charge retry/idempotency observable and must not claim that fail-closed can undo a successful
provider response. A fail-closed configuration applies to future admission when the store is unhealthy; charge
failure marks the store degraded and follows the bounded retry/recovery policy.

`HttpStateHandle` remains the process-level owner of `Arc<dyn BudgetStore>`, so reload publication never rebuilds
or clears the store. `HttpStateHandle::new()` keeps the in-process default; production assembly gains an explicit
constructor that accepts the selected store.

## 5. Configuration and failure policy

The local bootstrap gains an optional process-level quota storage section, separate from
`config_source` and from route `token_budget` policy. Shape is backend-tagged and closed:

```yaml
quota_store:
  type: etcd                 # local | etcd | redis
  namespace: nico/prod       # bounded printable label/prefix
  on_unavailable: fail-closed # required for etcd/redis; fail-closed | fail-open
  timeout_ms: 250
  etcd:
    endpoints: [https://etcd-0.internal:2379]
    key_prefix: /nico/quota/
    tls: ...
    auth: ...
  # or:
  # redis:
  #   endpoints: [rediss://redis.internal:6379]
  #   key_prefix: nico:quota:
  #   tls: ...
  #   auth: ...
```

This YAML is a shape sketch; exact fields and whether `local` is represented by omission are settled by schema
implementation. The requirements are fixed:

- omission selects the existing in-process store and needs no failure policy;
- `etcd` and `redis` require an explicit `on_unavailable`, so the deployment chooses fail-open/closed rather than
  inheriting an undocumented default;
- backend-specific endpoint, TLS, auth, namespace/prefix and operation timeout are bootstrap facts, never remote
  resource facts;
- invalid credentials, endpoint shape, namespace or policy fail startup/config validation before requests use a
  partially constructed store;
- runtime unavailable/timeout/conflict/corrupt errors become domain errors. `fail-closed` returns a new stable 503
  code such as `quota_store_unavailable`; `fail-open` forwards while emitting a degraded metric and structured log;
- `Exceeded` always remains 429 in either mode;
- charge failures never cause a second client response. They use the configured retry/idempotency policy and expose
  a metric; an implementation must document the bounded at-least-once window if it cannot provide durable retry.

## 6. Backend adapter contracts

### etcd

Add a quota-specific client seam for atomic transaction and lease/TTL operations. The adapter must use a single
server-side transaction/CAS path for each check and charge. The existing config client and mock are not extended
implicitly; quota tests get a dedicated fake and wire double that can assert transaction predicates, TTL and duplicate
charge behavior.

### Redis

Add a quota-specific client seam that can execute the check/charge script or transaction atomically and preserve
idempotency records with TTL. The adapter must not rely on a client-side read-modify-write. A Redis deployment with
cluster/replica behavior must document which node/transaction semantics the adapter requires; unsupported topology
is rejected during validation rather than silently weakening atomicity.

Both adapters expose the same domain errors, namespace isolation and metrics. Backend-specific response bodies,
credentials and addresses never cross into `HttpError` responses or request logs.

## 7. Compatibility, rollout and rollback

- Default bootstrap with no `quota_store` is byte/behavior compatible and keeps the current in-process LRU.
- Existing route `token_budget`, tenant mapping, 429 code, post-hoc overshoot and reload policy semantics remain.
- Shared mode is opt-in and feature-gated per backend; traffic builds continue to compile without LLM quota usage.
- The first shared release should document mixed-policy behavior during staggered config reload: policy remains
  request-supplied and replicas converge eventually, while the shared window is not reset.
- Rollback is configuration-first: remove `quota_store` to return to local mode, or disable the backend feature
  and keep the binary on local mode. Persisted shared keys are namespaced/versioned so a later schema can coexist
  or be explicitly invalidated.

## 8. Testing strategy

- Domain contract tests: local, etcd fake and Redis fake all agree on key, fixed window, exceeded, expiry,
  post-hoc overshoot, duplicate charge and typed errors.
- Concurrency tests: many independent store clients update one key; no increments are lost; the test asserts the
  documented post-hoc overshoot model rather than an impossible hard cap.
- Backend wire tests: etcd transaction/lease and Redis atomic script/TTL mapping, authentication, timeout and
  malformed-record handling. Tests use adapter seams and a local protocol double; no test depends on a developer's
  external services.
- Data-plane integration: two independent `HttpStateHandle`/store clients share tenant and subject windows; a
  passed budget prevents a provider request on the next admission; backend errors map according to configured policy.
- Lifecycle tests: store survives reload, shutdown cancels pending operations, and a failed candidate leaves LKG plus
  the existing shared store untouched.
- Existing full workspace gates, feature matrix, dependency boundary and 500-line checks remain mandatory.
