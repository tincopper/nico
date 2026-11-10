# 执行计划：自报面（`nico-metrics` + metrics 监听器）

前置：本文件必须在 `task.py start` 之前与 `prd.md` / `design.md` 一起通过评审。

## 检查点

- **A（步骤 1–3）**：`nico-metrics` 能力层完成且可脱离 HTTP 单测；`servable()` 就位、请求指标接上。此时能力层已是可交付的库，回滚成本为零。
- **B（步骤 4–6）**：传输层接通——配置段、适配器、装配、配置侧指标。此后只做测试与文档。
- **收口（步骤 7–9）**：集成测试、依赖边界、spec 同步、五条基线。

## 步骤

### 1. 骨架与依赖

- [ ] `[workspace.dependencies]` 追加三条精确 pin 的条目：`metrics = "=0.24.6"`、`metrics-exporter-prometheus = { version = "=0.18.3", default-features = false }`、本地成员 `nico-metrics = { version = "0.0.0", path = "crates/nico-metrics" }`。
      `default-features = false` 是硬要求：默认 feature 会拉进 hyper / tokio / rustls，与 Pingora 形成第二套网络栈。
- [ ] 新建 `crates/nico-metrics/Cargo.toml`：crate 文档注释、`[lints] workspace = true`、依赖只有 `nico-core`、`metrics`、`metrics-exporter-prometheus`、`serde_json`、`thiserror`。**没有 pingora、没有 tokio、没有 nico-http**。
- [ ] `crates/nico-server/Cargo.toml` 加 `nico-metrics` 与 `metrics`（配置侧指标用 facade）。
- [ ] `crates/nico-http/Cargo.toml` 只加 `metrics`（facade）。**不得**加 `nico-metrics`。
- [ ] 每条新依赖加一行注释说明它为什么在这一层（仓库惯例）。
- 验证：`cargo check --workspace --all-features`；`cargo tree -p nico-metrics --prefix none | grep -E 'pingora|tokio|nico-http'`（必须无输出）；`cargo deny check licenses bans`（覆盖全部传递依赖的许可证，研究约束 10 的残留风险在这里收口）；`cargo tree -d | head`。

### 2. `nico-metrics` 能力层

- [ ] `src/lib.rs`：`#![deny(missing_docs)]` + crate 文档（说明它是能力层：不监听、不依赖框架）+ `Metrics`、`install()`、`Readiness`、`Response`、三个路径常量。
- [ ] `src/recorder.rs`：`static RECORDER: OnceLock<PrometheusHandle>`；`install() -> Result<Metrics, MetricsError>` 幂等（已安装即返回既有 handle，只有首次安装可能失败）；`Metrics::render(&ConfigStatus) -> String`：先 `run_upkeep()` 再 `render()`，且在 render 之前现写 `config_degraded` 与 `config_snapshot_age_seconds`（`last_reload_unix_secs` 为 `None` 时跳过 age）。
- [ ] **upkeep 语义已取证**（docs.rs 0.18.3：`install_recorder` 不受 feature 门控，文档原文 "The caller is responsible for ensuring that upkeep is run periodically."）：`install_recorder()` 不启后台任务，且不跑 upkeep 时直方图渲染为空。因此 `Metrics::respond` 处理 `/metrics` 时先 `handle.run_upkeep()` 再 `handle.render()`，并在一行注释里写明这个顺序的理由。**不要**为此 spawn 任务或引入 tokio；`idle_timeout` 保持默认 `None`（不修剪空闲序列），并在模块文档里写下基数封顶于「文档出现过的路由名总数」。
- [ ] `src/health.rs`：`Readiness { servable, &ConfigStatus }` 的就绪投影——`live()` 返回 `ok`/`text/plain`，`ready()` 返回 JSON（`serde_json`，字段顺序固定便于断言），状态由 `servable` 决定。
- [ ] `src/response.rs`：`Response`（私有字段 + 四个访问器，含 `allow() -> Option<&'static str>`）与方法/路径路由：三个端点各一个分支，非 `GET` 得 `405`，`Allow` 头的取值由 `allow()` 声明，未知路径 `404`。`/metrics` 走 `render`，另两个走 `health`。
- [ ] 单元测试（`src/*/tests.rs` 或文件内 `#[cfg(test)]`）：**不用 `#[tokio::test]`、不起 socket**——直接构造 `Readiness` 调 `respond`，断言 `(status, content_type, body 关键字段)`。覆盖：live 恒 200、ready 两态、`405` 且 `allow() == Some("GET")`、`404`、`degraded && stale` 仍 200、`/metrics` 含 `config_degraded`；另加一条 upkeep 契约测试——先经 `metrics` facade 记一个直方图样本再渲染，断言样本出现在输出里（不跑 upkeep 直方图为空，这条测试就是该顺序的防回归门）。
- 验证：`cargo test -p nico-metrics`；`cargo clippy -p nico-metrics --all-targets -D warnings`；`cargo tree -p nico-metrics -e no-dev --prefix none | grep -c '^tokio'`（期望 0）。

### 3. `nico-http`：具名就绪谓词 + 请求指标

- [ ] `crates/nico-http/src/state.rs` 加 `HttpStateHandle::servable(&self) -> bool`（`current().is_some()` 的唯一具名定义，带 `///` 说明"这个进程现在能否服务一个请求"）。
- [ ] `crates/nico-http/src/proxy/hooks/request.rs:69` 的 `let Some(state) = app.state.current() else` 保持不变——该分支需要快照本身，不只是判定结果，先调 `servable()` 再取 `current()` 反而要读两次。在分支处加一行注释回指 `servable()`，并在 `servable()` 的文档里写明它就是该分支的谓词；「能否服务」的定义只落在 `servable()` 一处。
- [ ] `crates/nico-http/src/proxy/record.rs` 的 `emit()` 末尾（`status` / `route_id` / `elapsed_ms` / `error_category` 都已算好后）记 `gateway_http_requests_total` 与 `gateway_http_request_seconds`。
- [ ] 标签映射写成该模块内两个小函数：`status_class(u16) -> &'static str`、`error_label(Option<&ErrorCategory>) -> &'static str`；`route_id` 为空时用 `unmatched`。记录调用无条件（不加 `if installed` 分支）。
- [ ] `record.rs` 现 269 行，本步增约 20 行——若接近 300 行就把两个标签函数与记录语句移进 `record/metrics.rs`（新的子模块，仍属同一边界）。
- 验证：`cargo test -p nico-http --lib`；`cargo test -p nico-server --test http_routing`（既有日志断言不受影响）。

### 4. `metrics` 配置段

- [ ] `crates/nico-http/src/settings.rs` 的 `validate_listen` 提升为 `pub`（附文档说明它是"什么叫合法地址"的唯一定义），`HttpSettings::resolve` 内部继续用它。
- [ ] 新建 `crates/nico-server/src/bootstrap/metrics.rs`：`RawMetrics { listen: Option<String> }`（`deny_unknown_fields`）+ `resolve(http_listen: &str) -> Result<MetricsSettings, BootstrapError>`。同址检查在这里做，错误走 `BootstrapError::InvalidField { field: "metrics.listen" }`。
- [ ] `MetricsSettings` 字段私有、只由 `resolve` 构造，暴露 `listen(&self) -> &str`（与 `HttpSettings` 同一形态）。
- [ ] `bootstrap.rs`（现 387 行）加 `metrics: Option<RawMetrics>` 字段与 `BootstrapConfig::metrics() -> Option<&MetricsSettings>`。
- [ ] 测试放 `crates/nico-server/src/bootstrap/metrics/tests.rs`（`bootstrap/tests.rs` 已 459 行，不再加）。
- 验证：`cargo test -p nico-server --lib bootstrap::metrics`；`cargo test -p nico-http --lib settings`。

### 5. 传输层与装配

- [ ] 新建 `crates/nico-server/src/metrics.rs` + `metrics/{app.rs,service.rs}`：`MetricsApp: ServeHttp` 持 `Metrics`、`SnapshotHandle`、`Arc<HttpStateHandle>`；`service()` 建 `HttpServer::new_app` + `Service::new("metrics", ..)` + `add_tcp`。
- [ ] `(status, content_type, body)` → Pingora `Response<Vec<u8>>` 的映射只有一个函数：`StatusCode::from_u16` 失败时归 `500` 并记 error（不写 `expect`）；头只写三个来源——`content-type`、实测写入的 `content-length`、`reply.allow()` 为 `Some` 时的 `Allow`，不补任何别的头。`nico_metrics::Response` 用 `as Reply` 别名，避免与 Pingora 的 `Response` 重名。
- [ ] `server.rs`（现 194 行）：`if let Some(metrics) = bootstrap.metrics()` 分支里 ① 预检 `std::net::TcpListener::bind` 后立即释放，失败 → error 日志含地址 + `ExitCode::from(FAILURE_EXIT_CODE)`；② `nico_metrics::install()`，失败同样退出并给原因；③ `server.add_service(metrics::service(...))`。
- [ ] 两种情形各打一行 info：开启给地址，关闭写 `metrics surface disabled`。
- [ ] 本步约增 25 行；若 `server.rs` 接近 220 行，把预检与 registry 安装收进 `metrics::assemble(...)`，装配点只留一行。
- 验证：`cargo test -p nico-server --test http_shutdown --test pipeline`（现有套件零改动通过，证明默认关闭无损）。

### 6. 配置侧指标发出点

- [ ] `reload.rs`（459 行）记 `config_load_total{source,result}`——只加一条记录语句。
- [ ] `reload/publish.rs` 记 `config_reload_total{result}` 与 `config_compile_seconds`（复用该处既有计时，不新增 `Instant::now()`）。
- [ ] `reload/pacing.rs:60,70` 的 `ResyncRequired` 分支记 `config_watch_reconnects_total{source}`。
- [ ] 四个名字与标签在注释里各留一句"这个 counter 在这一刻加一"的说明；不加常量表。
- [ ] **文件预算**：`reload.rs` 只加 2 条语句；任何超出预算的冲动都意味着新逻辑该进 `reload/metrics.rs`，不是把 `reload.rs` 撑过 500。
- 验证：`cargo test -p nico-server --lib reload`；`find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'`。

### 7. 集成测试

- [ ] `crates/nico-server/tests/support/http.rs` 增**新增式**辅助（不改 `write_bootstrap` 的位置参数签名，它被 20+ 文件调用）：`write_bootstrap_with_metrics(directory, port, metrics_port) -> BootstrapConfig`、`RunningServer::metrics_address() -> Option<SocketAddr>`。
- [ ] 新建 `crates/nico-server/tests/ops_surface.rs`，用例：
      1. 未配置 `metrics`：数据面上 `/health/ready` 是普通 404，`metrics_address()` 为 `None`。
      2. 配置 `metrics`：三端点可用；数据面同一路径仍是 404（自报面既未被文档遮蔽，也无法遮蔽文档）。
      3. 非 `GET` 请求 `/health/ready` 得 `405` 且响应头含 `Allow: GET`，未知路径得 `404`（自报面端口上）。
      4. `/health/live` 在没有快照时就 200（用一个永不发布快照的 fake source）。
      5. `/health/ready` 从 503 到 200 的转变与 `generation` 一致；LKG 启动（`degraded && stale`）仍 200 且字段如实。
      6. 失败发布后：`config_reload_total{result="error"}` 增加、`config_degraded` 为 1、readiness 仍 200。
      7. 一次真实请求后：`gateway_http_requests_total` 出现对应 `route_id` / `status_class` / `error_category` 组合，`gateway_http_request_seconds` 计数增加；两次 scrape 的 `config_snapshot_age_seconds` 递增。
      8. `metrics.listen` 与 `http.listen` 同址 → 启动拒绝（配置层，由步骤 4 的单测覆盖；集成测试只保留端口被占用的退出码用例）。
- [ ] HTTP 抓取用现有 `client::exchange`，不引入 reqwest。
- 验证：`cargo test -p nico-server --test ops_surface`。

### 8. 依赖边界与角色

- [ ] 断言三条边界（写进 `crates/nico-server/tests/structure.rs` 或沿用既有的依赖断言测试）：`nico-metrics` 的依赖树不含 pingora / tokio / nico-http；`nico-http` 的依赖树不含 `nico-metrics` 与 `metrics-exporter-prometheus`；`nico-core` 不含 `metrics`。
- [ ] 角色矩阵：`cargo check --no-default-features`、`cargo check -p nico-server --no-default-features --features traffic`、`cargo check --workspace --all-features` 全通过；`traffic` 角色下运行一次 `ops_surface`（或 `role_traffic` 里加一条端点断言）。
- [ ] `cargo tree -p nico-server -e no-dev | grep -c hyper` 与基线对比，确认没有第二套。
- 验证：见上三条命令 + `cargo tree -d`。

### 9. 文档与 spec

- [ ] `.trellis/spec/backend/logging-guidelines.md` 的 Metrics 节改写为**已实现**：删去"arrive with the phase-1 data plane"的历史语气，写清自报面的位置（`metrics.listen`，默认关闭）、三个端点、八个指标族（名字/类型/标签/唯一写出点）、`gateway_` 命名模板只适用于基线列表之外的新指标、自报面关闭即不装 recorder。两处契约扩宽随改写显式入文：`status_class` 取值集从 `2xx/4xx/5xx` 扩为 `1xx/2xx/3xx/4xx/5xx/none`（`499` 归 `4xx`、`0` 归 `none`），`config_watch_reconnects_total` 增加标签 `source`（语义映射为收到 `ResyncRequired`）。
- [ ] `.trellis/spec/backend/directory-structure.md`：依赖图加 `nico-server ──> nico-metrics ──> nico-core`；"Where new code goes" 加一行（指标渲染与就绪投影 → `nico-metrics`；监听器与 Pingora 适配 → `nico-server`）；目录树加 `crates/nico-metrics/src/{lib,recorder,health,response}.rs` 与 `crates/nico-server/src/metrics/`。
- [ ] `README.md`（工作树里已有未跟踪版本）若含 bootstrap 示例，补 `metrics.listen` 一段；没有示例则不动。
- [ ] 任务 `implementation-notes` 记录检查点 A/B 的实际结果。

## 收口验证

```bash
cargo test --workspace --all-features --locked
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo deny check
cargo check --workspace --no-default-features
cargo check -p nico-server --no-default-features --features traffic
cargo test -p nico-metrics
cargo test -p nico-server --features llm --test ops_surface
# 依赖边界（都必须是断言，不是人眼检查）
cargo tree -p nico-metrics --prefix none | grep -E 'pingora|tokio|nico-http'   # 期望无输出
cargo tree -p nico-http --prefix none | grep -E 'nico-metrics|metrics-exporter' # 期望无输出
cargo tree -p nico-core --prefix none | grep -E 'metrics'                       # 期望无输出
# 文件预算
find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'
```

## 回滚点

- 步骤 1–3 后（检查点 A）：删 `metrics` 段即回到今天的行为；`nico-metrics` 作为独立 crate 可以原样保留，没有消费者也不会有运行时成本。
- 步骤 4–6 后（检查点 B）：指标记录点在无 recorder 时是 no-op，回滚不需要改热路径。
- 任一步骤发现必须让 `nico-metrics` 依赖 `nico-http` 或 Pingora：停下来回到设计——那否掉了本 crate 存在的理由，要么改回"全放 `nico-server`"，要么接受 crate 自带传输层。
