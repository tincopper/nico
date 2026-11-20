# 执行计划：metrics 监听 TLS 终止

## 前置

- 需求与设计：同目录 `prd.md`、`design.md`；冲突时以 PRD 的验收标准为准。
- 约定：Rust 走 `.trellis/spec/backend/`；不提交（用户明确要求时才 commit）。
- 顺序有意义：先做 `nico-http` 的公共入口（步骤 2），再做 bootstrap（步骤 3），否则 metrics 会先
  复制一份 TLS 构建逻辑。

## 清单

### 1. 收口共享的 bootstrap 原始块

- [x] 新建 `crates/nico-server/src/bootstrap/inbound_tls.rs`：从 `bootstrap/http/listener.rs` 移入
      `RawTlsIdentity`、`section` 反序列化与 `resolve`；`resolve(&self, field: &str)` 用字段前缀
      拼出 `{field}.cert_file` / `{field}.key_file`。
- [x] `bootstrap/http.rs` 改为经由 `super::inbound_tls` 解析，前缀 `http.listeners[{index}].tls`。
- [x] 删除 `bootstrap/http/listener.rs`，`bootstrap.rs` 声明新模块。
- [x] `cargo test -p nico-server --lib bootstrap` 绿，错误字段文案不变。

验证：`cargo test -p nico-server --lib`；`rg "http.listeners\[0\].tls" crates/nico-server` 确认既有
断言仍匹配。

### 2. `nico-http`：把 TLS settings 构建变成公共入口

- [x] `service.rs`：`tls_settings` 改为 `pub fn listener_tls(identity: &InboundTls, alpn: ListenerAlpn)
      -> Result<TlsSettings, ServiceBuildError>`；`ListenerAlpn::{Http1, Http2Or1}` 带文档；
      `Http2Or1` 调用 `enable_h2()`，`Http1` 调用 `set_alpn(ALPN::H1)`。
- [x] `service()` 的 TLS 分支传 `ListenerAlpn::Http2Or1`。
- [x] `lib.rs` 导出 `ListenerAlpn` 与 `listener_tls`，crate 文档一句话说明两个消费者。
- [x] 加/改单测：`Http2Or1` 与 `Http1` 产出的 settings 在 ALPN 上不同（用 `TlsSettings` 的可观测
      行为或对 `build()` 的断言；若不可直接观测，则在 `tests` 里用 `set_alpn` 的等价性说明）。

验证：`cargo test -p nico-http --lib`；`cargo test -p nico-server --test tls_inbound`（数据面不回归）。

### 3. bootstrap：`metrics.tls`

- [x] `bootstrap/metrics.rs`：`RawMetrics` 增加 `#[serde(default, deserialize_with =
      "super::inbound_tls::section")] tls`；`MetricsSettings` 存 `Option<InboundTls>` 并提供
      `tls()` 访问器；`resolve` 在地址规则之后解析块，前缀 `metrics.tls`。
- [x] `bootstrap/metrics/tests.rs` 增加：合法配对通过、空 `tls:`、缺 `key_file`、未知字段、地址
      冲突仍在 `metrics.listen`；沿用既有 fixture/临时文件写法。
- [x] `bootstrap/tests.rs` 的 `metrics` 相关断言不修改即通过（回归证明）。

验证：`cargo test -p nico-server --lib bootstrap::metrics`。

### 4. 服务装配

- [x] `metrics/listener.rs`：`service()` 返回 `Result<Service<HttpServer<MetricsApp>>,
      ServiceBuildError>`；有 `tls()` 时 `listener_tls(identity, ListenerAlpn::Http1)` +
      `add_tls_with_settings`，否则 `add_tcp`。
- [x] `metrics.rs::assemble` 不变（preflight bind + recorder）。
- [x] `server.rs`：处理 `service(...)` 的 `Err`——记 `tracing::error!`，返回
      `ExitCode::from(FAILURE_EXIT_CODE)`，与数据面/assemble 同一出口；启动日志补 `metrics.tls`
      字段。
- [x] 单测（若不依赖真实进程）：`server.rs` 的既有装配测试若断言日志/退出码，补一个 TLS 装配失败
      分支的覆盖；否则在 E2E 里覆盖。

验证：`cargo check -p nico-server --all-targets`。

### 5. E2E：TLS 自报面

- [x] `tests/support/http/tls_bootstrap.rs` 增加 `write_bootstrap_metrics_tls(directory, port,
      metrics_port, cert, key)`，与既有 renderer 同源（不写第二份模板）。
- [x] 新建 `tests/metrics_tls.rs`（一个 server、一个文档）：
      - TLS 客户端（`support::inbound_tls::TlsClient`，`localhost` fixture + `test-root`）取
        `/metrics`：状态行 200、`content-type` 以 `text/plain` 开头；
      - 同连接/新连接取 `/health/ready`：200 且 body 是 readiness JSON；
      - 客户端同时提供 `h2` 与 `http/1.1` → `client.alpn() == Some(b"http/1.1")`；
      - 明文 TCP 连同一端口发 `GET /metrics` → 读不到 HTTP 响应（连接关闭/握手失败），断言方式沿用
        `tls_inbound` 的明文探针。
- [x] 断言失败信息写清"传输事实"而不是状态码。

验证：`cargo test -p nico-server --test metrics_tls`。

### 6. 文档与示例

- [x] `README.md`：bootstrap 示例的 `metrics` 段补注释掉的 `tls` 块；要点段补一句（可选 TLS、启动期
      加载、只协商 h1、换证书重启）；可观测性首段把"在 `metrics.listen` 上独立监听"补上"可选终止 TLS"。
- [x] `config/bootstrap.yaml` 的 `metrics` 注释同步。
- [x] `deploy/bootstrap.yaml` 的 metrics 注释说明可加 `tls` 块（样例保持明文，避免镜像缺证书）。
- [x] `docs/nginx-migration.md`：§2 的 `metrics` 段落与 §5 的 TLS 行更新，删掉"明文"这个绝对表述。
- [x] `README 当前限制` 不新增条目：本次没有留下新的能力缺口；若发现 metrics TLS 与数据面共享
      限制（无 mTLS/无 SNI/无热加载）表述缺失，在可观测性一段点明。

验证：`rg "metrics" README.md docs/nginx-migration.md config/bootstrap.yaml deploy/bootstrap.yaml`，
逐条确认没有"只能明文"的陈述；`docs/nginx-migration.md` 的 YAML 示例仍可被 `Document::parse` 之外
的 bootstrap 校验接受（bootstrap 只做形状校验，certs 用 fixture 实测见步骤 5）。

### 7. 质量门

- [x] `cargo +nightly fmt --all`（不可用时 `cargo fmt --all`）。
- [x] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`。
- [x] `cargo test --workspace --all-features --locked`。
- [x] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [x] `cargo check --workspace --all-targets` 与 `--no-default-features --features traffic` 组合。
- [x] 文件预算：`find crates -name '*.rs' | xargs wc -l | sort -rn | head`，无新增超 500 行文件。
- [x] 更新 `.trellis/spec/backend/directory-structure.md` 的 bootstrap 文件清单（`inbound_tls.rs`、
      metrics 的 TLS 终止）与 `metrics.rs` 职责描述。

## 复审门槛

- 步骤 2 后复审一次：公共入口的签名与错误面是否让 `nico-server` 真的不需要 Pingora 类型。
- 步骤 5 后复审一次：E2E 是否证明"明文被拒在握手阶段"而不是"明文拿到 4xx"。
- 步骤 6 后复审一次：文档中是否还残留 `metrics` 只能明文的表述。

## 回滚点

| 回滚到 | 方式 |
|---|---|
| 无 TLS 支持 | 回退步骤 2–5 的产品代码；bootstrap 里删掉 `tls` 块即回到明文 |
| 只有 bootstrap 校验、不服务 TLS | 回退步骤 4–5（校验先行，不会出现"配置被接受但不生效"） |
| 只有共享模块重构 | 回退步骤 1（纯重构，行为不变，可单独保留） |
