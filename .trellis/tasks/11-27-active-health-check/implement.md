# 实施：上游主动健康检查

## 阶段 0：传输能力验证（必须先通过）

- [ ] 对照 pinned Pingora 0.9.1 源码，确认在不构造 `ProxyHttp` 下游请求的情况下，能否从 BackgroundService 建立独立 HTTP/1 upstream 请求、发送 method/path/Host、读取 status head 并在 timeout/drop 时关闭连接。
- [ ] 验证探测可使用与正常 upstream 完全一致的 TLS connector/trust roots 和 SNI/authority；证书校验必须开启，不能为了 probe 绕过。
- [ ] 写一个最小测试 client/loopback mock，证明纯明文与 TLS 两种 peer 的 request/response head 流程及 timeout。
- [ ] 若 Pingora 无可复用 outbound client seam：停止产品实现，记录 capability finding 并返回规划门；不得静默新增 reqwest/hyper 或自建 TLS 信任逻辑。本任务方案将据此重新审阅。

**检查点 A**：transport gate 有明确的源码证据和可运行的 loopback proof；失败则停在规划，不做后续阶段。

## 阶段 1：配置 schema（nico-core）

- [ ] `config/raw.rs`：closed `RawHealthCheck` 接入 `RawUpstream`，字段：method/path/expect/interval_ms/no_traffic_interval_ms/timeout_ms/unhealthy_threshold/healthy_threshold。
- [ ] `config/upstream.rs`：新增 `HealthCheck`、`ProbeMethod`、`ExpectedStatus`；Upstream 暴露 `Option<HealthCheck>`。
- [ ] `config/semantic`：method GET/HEAD；expect non_5xx/2xx；interval/timeout/no_traffic interval `1..=600_000`；fall/rise `1..=100`；path bounded printable ASCII、`/` 开头、禁 `?/#/whitespace/control`。
- [ ] 省略 health_check = None；省略 no_traffic interval 使用 interval；省略 fall/rise 均为 1；初始状态固定 Healthy。
- [ ] 配置变更继承规则在 `design.md` 定义的 equality 条件下测试：完全相同策略继承 active state；策略改动/启停切换重置 active state；passive state 仍按 MemberId 继承。

**检查点 B**：`cargo test -p nico-core --lib` 全绿，现有 upstream 文档解析无改动。

## 阶段 2：主动状态机（nico-http）

- [ ] 在 `MemberState` 增加独立 `ActiveHealth`；不改 passive `Health::Condition` 的 `failures/ejected_at/probing_since` 语义。
- [ ] active state：Initial(H) / Healthy / Unhealthy、fall/rise 连续计数、single in-flight claim、last probe/traffic 时间；锁内无 I/O/await/allocation。
- [ ] member admission 取 active/passive 交集：未启用 active = 兼容旧行为；启用时只允许 ActiveHealthy 且 passive available/admit。
- [ ] 拆分 outcome writer：真实请求仅写 passive state；probe 仅写 active state；各自的 transition logger/counter 明确归属。
- [ ] 更新 health.rs 文档，不再绝对声称只从真实流量学习；补 active/passive 互不清零、fall/rise、被动 ejection 不被 active success 清除的测试。

**检查点 C**：`cargo test -p nico-http --lib` 全绿；既有 passive tests 不删、不弱化。

## 阶段 3：探测执行器（nico-http）

- [ ] 新模块只负责单次 probe 与调度事实，不自行 spawn；server 提供 tick/current table 调用它。
- [ ] 对每个 health_check 成员按配置构造 upstream peer；HTTP Host/TLS SNI 使用 Upstream.logical host，连接地址使用 MemberId.address/port；传输 gate 阶段 0 已证明 TLS 信任完全复用。
- [ ] 发送 method/path，不带 credential、body、用户头、plugin、quota；读取 response head 即结束。
- [ ] timeout 截止后 drop client session；每成员 single in-flight；所有错误映射至 ProbeResult（connect/timeout/5xx/status_mismatch）。
- [ ] 对每个结果更新 active fall/rise；只在状态转换时写 warn/info；每个 probe 更新 `gateway_upstream_probe_total`，标签文档有界。
- [ ] mock 单测覆盖 2xx/4xx/5xx、connect error、timeout、TLS 验证失败、无 body 读取、Host/SNI 与 path。

**检查点 D**：`cargo test -p nico-http --lib` 全绿，dependency boundary 无新增运行时依赖。

## 阶段 4：BackgroundService 调度（nico-server）

- [ ] 新 `HealthService` 仅当至少一个 upstream 启用了 health_check 时装配；持有共享 HttpStateHandle、CancellationToken、probe executor。
- [ ] 在 Pingora runtime 中调度，不在 nico-http 内 spawn；shutdown 取消 tick 并有界等待探测任务。
- [ ] 每轮从当前 immutable SelectionTable 获取新成员；不缓存过期表；配置 generation 变化自然切到新策略与成员集。
- [ ] 实现 interval/no-traffic interval；首轮按 hash(upstream, endpoint) 在 interval 内错峰；upstream 间并发受限、同 upstream 串行。
- [ ] probe 沿旧 generation MemberState 完成，只有 MemberId 完全继承的 Arc 才对当前成员可见；成员移除后的 Arc 随任务 drop。
- [ ] 单测 tick/no-traffic/错峰/并发上限/shutdown/reload。

**检查点 E**：`cargo test -p nico-server --all-features --lib` 全绿，server 不启动第二个 runtime。

## 阶段 5：端到端、文档与交付

- [ ] mock upstream E2E：零真实请求时持续 probe；fall 次失败后从 rotation 移除；mock 恢复后 rise 次成功回池。
- [ ] `non_5xx`：4xx 算通过；`2xx`：4xx 算 mismatch；5xx/connect/timeout 分别计 probe result。
- [ ] 验证 active healthy 不能清除 passive ejection，真实请求的 passive answer/failure 不改变 active counters。
- [ ] README upstream health_check 示例、初始 Healthy、fall/rise、no traffic interval、TLS Host/SNI、探测不带凭据、provider 不适用、指标说明。
- [ ] 更新 metrics spec / quality docs；dependency boundaries、feature matrix、500-line budget。

**检查点 F**：全量 workspace suites、feature matrix 与 gates 通过。

## 验证命令

```bash
cargo test -p nico-core --lib
cargo test -p nico-http --lib
cargo test -p nico-server --all-features --lib
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
cargo check --workspace --no-default-features --locked
cargo check -p nico-server --no-default-features --features traffic --locked
cargo test -p nico-server --no-default-features --features traffic --test role_traffic --locked
find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'
```

## Rollback points

- 阶段 0 失败：无产品实现，回规划修改传输方案。
- 阶段 1/2：health_check 缺省关闭；回滚 schema 和 ActiveHealth 不改变现有文档行为。
- 阶段 3/4：删除 BackgroundService 装配即停止所有主动探测，保留 passive path 原样。
- 资源文档删除 health_check 可即时回到纯被动；不需要状态迁移。
