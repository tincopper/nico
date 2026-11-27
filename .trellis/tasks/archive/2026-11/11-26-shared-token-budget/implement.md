# 实施：跨副本共享 token budget 与 tenant quota

## 阶段 0：契约冻结与边界检查

- [ ] 复核 PRD/design 的 post-hoc 语义：不做 reservation，不承诺零超额硬上限；charge 允许把窗口推过 limit。
- [ ] 复核公共 domain seam 的 owner：`nico-http` 公开异步 quota contract；具体 etcd/Redis client 不进入其依赖树。
- [ ] 固定 backend namespace、记录版本、charge id、expiry/wall-clock 形状和 `BudgetStoreError` 映射。
- [ ] 固定 bootstrap 形状：省略 = local；shared backend 必须有 `on_unavailable`；backend-specific 块 closed。
- [ ] 决定 adapter 放置（优先独立 `nico-quota-storage` crate）并核对 Cargo feature / dependency boundaries。

**检查点 A**：设计中的接口能表达 `Exceeded`、typed store error、幂等 charge 和 async I/O；没有 etcd/Redis 类型泄漏到 nico-http。

## 阶段 1：nico-http domain contract 与本地实现迁移

- [ ] 将 `BudgetKey`、`BudgetPolicy`、check/charge 请求与结果、store error 从 crate-private 细节整理为可实现的 domain contract；同步修正 quota 文档中已过时的 subject-only 表述。
- [ ] 将 `BudgetStore` 改为 async、错误感知接口；保留 `InProcessBudgetStore` 的固定窗口/LRU 语义并适配新契约。
- [ ] 为本地实现增加稳定 charge id 的重复记账幂等测试，确保现有本地行为可回归。
- [ ] `HttpStateHandle` 保留默认 local 构造，新增显式注入 `Arc<dyn BudgetStore>` 的构造入口；reload 不重建 store。
- [ ] 为 `HttpError` 增加独立 quota store unavailable 映射（503），并保留 exceeded 的 429 contract。
- [ ] 把 `upstream_peer` 的 check 改为 await；把 response logging / record_tokens 的 charge 改为可等待路径，明确响应已提交后的错误处理、重试和指标。

**检查点 B**：`cargo test -p nico-http --lib`，domain contract 与现有本地 token budget/tenant tests 全绿。

## 阶段 2：bootstrap schema 与 server 装配

- [ ] 在 `nico-server::bootstrap` 增加 `quota_store` 独立 section：local/etcd/redis、namespace/prefix、timeout、`on_unavailable` 及各后端 TLS/auth/endpoints。
- [ ] 为 backend-specific unknown fields、必填策略、范围、空 namespace、错误 credential 引用和 feature 未编译场景增加解析/验证测试。
- [ ] 在 `nico-server` 装配层构造 local 或 shared store，并把同一个 Arc 注入 `HttpStateHandle`；pipeline-only `serve` 保留 local 默认路径。
- [ ] 启动期验证静态配置、连接材料和资源限制；运行期连接错误通过 domain error 进入配置的 fail-open/fail-closed 处理。
- [ ] 更新 bootstrap 示例、README 和 backend spec，明确多副本共享、post-hoc overshoot 与故障策略。

**检查点 C**：bootstrap/unit tests、server construction tests、traffic/no-default feature check 全绿。

## 阶段 3：quota storage contract crate 与 fake

- [ ] 创建独立 storage owner（若阶段 0 证明 server 内边界更合适，按同一接口落在 `nico-server/src/quota/`），定义 adapter-facing seam，不让协议类型进入 domain contract。
- [ ] 实现 deterministic fake store：原子窗口更新、expiry、duplicate charge、namespace、typed errors、故障注入。
- [ ] 契约测试套件对 local/etcd/Redis adapter 复用：并发更新不丢增量、post-hoc 越额、重复 charge 不重复扣费。
- [ ] 每个新 Rust 文件控制在 500 行以内；按 data model / protocol / adapter / test 场景拆分。

**检查点 D**：fake contract suite 全绿，dependency boundary tests 确认 nico-http 不链接外部客户端。

## 阶段 4：etcd adapter

- [ ] 新增 quota-specific etcd client seam，表达 transaction/CAS、lease/TTL、server-time/expiry 和 typed transport errors。
- [ ] 实现 etcd adapter：check 和 charge 各自服务端原子；charge 记录幂等 id；损坏记录拒绝而不清零。
- [ ] 复用已有 etcd TLS/auth/runtime 经验但不扩展 `ConfigSource`/`EtcdClient` API；按 feature 门控。
- [ ] 新增 etcd protocol mock/wire tests：事务谓词、TTL、重复 charge、认证、超时、不可用和 malformed record。

**检查点 E**：etcd adapter contract + wire tests 全绿，`cargo tree` 无不受控的依赖栈漂移。

## 阶段 5：Redis adapter

- [ ] 选定并锁定 Redis client、TLS/auth 和 topology 支持范围；不支持的集群/replica 语义在 bootstrap 拒绝。
- [ ] 实现服务端脚本或事务：check/charge 原子、TTL、namespace 和 charge id 幂等；不做客户端 read-modify-write。
- [ ] 新增 Redis protocol fake/wire tests：脚本映射、过期、duplicate charge、认证/超时/损坏记录和 typed errors。

**检查点 F**：Redis adapter contract + wire tests 全绿；默认/traffic 构建不引入 Redis 依赖。

## 阶段 6：数据面多副本与故障 E2E

- [ ] 两个独立 nico state/store client 共享 tenant key：第一次请求跨实例成功，额度耗尽后下一次 admission 429，provider 不收到请求。
- [ ] 同样覆盖无 tenant subject key、不同 route 隔离和两个不同 tenant 隔离。
- [ ] 注入 unavailable/timeout/conflict/corrupt：fail-closed 返回 503 `quota_store_unavailable`，fail-open 继续并产生 degraded metric/log；任何模式都不返回 429 伪装错误。
- [ ] provider 成功后重复 logging/charge 只计一次；charge 失败不重写已提交响应，后续恢复符合有限重试/at-least-once 契约。
- [ ] reload 候选失败保留 LKG，shared store 与累计窗口继续存活；shutdown 取消挂起外部操作。

**检查点 G**：nico-server quota E2E 全绿，现有 LLM convert、tenant、WASM、traffic 场景无回归。

## 阶段 7：全量质量门与交付

- [ ] `cargo +nightly fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo test --workspace --all-features --locked`
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`
- [ ] `cargo deny check`
- [ ] feature matrix：no-default、traffic、llm、all-features，以及 backend feature combinations。
- [ ] dependency boundaries：nico-http 不依赖 etcd/Redis client；traffic build 不带 LLM/shared backend 不必要依赖。
- [ ] `find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'`
- [ ] 复核 README、bootstrap sample、error/log/metrics spec 与实现一致。

## Rollback points

- 阶段 1 完成后可回滚到同步 local store；不改变默认配置行为。
- 阶段 2 完成后 shared config 尚未默认启用，删掉 quota_store section 即回到 local。
- 每个 backend adapter 独立 feature；单个 adapter 失败不阻塞另一 adapter 或 local build。
- 共享持久化 key 使用 versioned namespace，禁止通过回滚二进制静默解释不兼容记录。
