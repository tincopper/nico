# 执行计划：etcd 配置源 TLS 与认证

## 前置

- 需求与设计：同目录 `prd.md`、`design.md`；冲突以 PRD 验收标准为准。
- 顺序有意义：先拆 bootstrap（步骤 1），再加 schema（步骤 2），再动客户端（步骤 3），
  最后才是装配与端到端（步骤 4–5）。反过来做会让 `source.rs` 先超预算。
- 约定：Rust 走 `.trellis/spec/backend/`；不提交（用户明确要求时才 commit）。

## 清单

### 1. bootstrap 拆分（纯搬迁，行为不变）

- [ ] 新建 `crates/nico-server/src/bootstrap/source/bounds.rs`：`RawSchemaBounds`、`maximum`、
      `out_of_range` 原样移入；`source.rs` 保留 `mod bounds;` + `use`。
- [ ] `bootstrap.rs` 声明 `source/`（`source.rs` 存在时用子模块，不新建 `mod.rs`）。
- [ ] `cargo test -p nico-server --lib bootstrap` 绿，错误字段文案不变。

### 2. schema：`config_source.tls` 与 `config_source.auth`

- [ ] 新建 `crates/nico-server/src/bootstrap/source/etcd.rs`：`RawEtcdSource` 增加 `tls`/`auth` 两个
      可选块（`#[serde(default, deserialize_with = ...)]` 以区分"省略"与"写了空块"）；
      `validate_endpoints` 与 `validate_endpoint` 移入，加入 scheme × tls 矩阵与混合 scheme 规则。
- [ ] `EtcdTls` / `EtcdAuth` 值类型（只存路径与引用），`SourceSelection::Etcd` 带上它们。
- [ ] 启动期读取：CA 用 `nico_http::validate_bundle`；客户端身份用
      `nico_http::validate_server_identity`（调用点写清"名字里的 server 指证书方向，不指字段归属"）；
      字段名 `config_source.tls.*`。口令引用只校验形状，不解析。
- [ ] `bootstrap/tests/etcd_source.rs` 补正负例（PRD AC1/AC2 的前半）。
- [ ] 500 行预算复核：`wc -l` 三个文件都留有余量。

验证：`cargo test -p nico-server --lib bootstrap::tests::etcd_source`。

### 3. `nico-config-etcd`：连接选项 + 协议替身

- [ ] `client/real.rs`：`EtcdTlsOptions` / `EtcdCredentials` / `EtcdConnectOptions`（`Default` 为无 TLS
      无凭据）；`GrpcEtcdClient::connect(endpoints, options)` 构造 `etcd_client::ConnectOptions`，
      `with_tls`/`with_user` 只在有值时调用；失败仍走 `classify`。
- [ ] `Cargo.toml`：`client = [..., "etcd-client/tls"]`，并更新 crate 文档里"TLS 属于 client 实现"的
      句子；确认没有引入 `tls-roots`。
- [ ] `client/mock_server.rs`：`MockServer::builder()` + `.identity(cert, key)` + `.credentials(user, password)`；
      `start()` 走 builder；鉴权按 etcd 的真实语序实现——`Authenticate` 只在凭据匹配时签发 token，
      KV/Watch 前的 interceptor 只接受该 token，不匹配回 `Status::unauthenticated`（只检查
      `authorization: Basic ...` 的实现是错的：`etcd-client` 的 `Authenticate` 调用不带这个头）。
- [ ] 测试：TLS mock 上 get/watch；需要凭据的 mock 正确/错误/缺失；明文客户端连 TLS mock 得 `Connect`。
- [ ] `cargo tree -p nico-config-etcd --features client | grep -E 'rustls-native-certs|tonic'` 记录结果。

验证：`cargo test -p nico-config-etcd`（含 `--features mock-server`）。

### 4. 装配：`source_build.rs`

- [ ] `SourceBuildError::Setting { field, reason }`，`category()` = `Client`，`Display` 不含口令或 PEM。
- [ ] etcd 分支：解析口令（`env:`/`file:`，逐字节读）→ 读 CA/客户端身份字节 → 构造
      `EtcdConnectOptions` → `connect(endpoints, options)`。
- [ ] 单测：口令缺失/不可读、CA 在 bootstrap 之后被删、身份被换 → 各自字段名；不走真实网络的部分
      用不可达端点断言 `Connect` 分类。

验证：`cargo check -p nico-server --all-targets --features etcd`。

### 5. 端到端

- [ ] `crates/nico-server/tests/etcd_bootstrap.rs` 增加一个 TLS + 凭据变体：bootstrap 写 `https://`
      端点 + `tls`（fixture CA、`domain_name: localhost`）+ `auth`（`file:` 口令）；mock 用同一份
      fixture 证书并要求同一凭据；断言文档进入发布、数据面可服务。
- [ ] 负例：TLS 端点但 mock 要求凭据而 bootstrap 未配置 → 启动失败、退出码与错误分类正确。

验证：`cargo test -p nico-server --features etcd --test etcd_bootstrap`。

### 6. 文档与 spec

- [ ] README `config_source` 示例补 `tls`/`auth`，说明信任根只来自 `ca_file`、口令用引用、
      `domain_name` 一个客户端一个名字；"当前限制"里"etcd 的 TLS/认证凭据尚未暴露"改为已支持，
      并留下平台信任库不参与这一条。
- [ ] `.trellis/spec/backend/database-guidelines.md`：etcd 源的传输与凭据契约。
- [ ] `.trellis/spec/backend/error-handling.md`：`config_source.tls.*` / `config_source.auth.*` 的
      启动错误字段与"构建期 Setting 变体"。
- [ ] `.trellis/spec/backend/directory-structure.md`：bootstrap 的 `source/` 拆分与
      `nico-config-etcd` 的选项类型。
- [ ] `crates/nico-server/tests/dependency_boundaries.rs`：若新增依赖边（tonic TLS 等）需要断言，
      按现有风格补。

### 7. 质量门

- [ ] `cargo +nightly fmt --all`（不可用时 stable）。
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`。
- [ ] `cargo test --workspace --all-features --locked`。
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [ ] feature matrix：`cargo check --workspace --no-default-features`、
      `cargo check -p nico-server --no-default-features --features traffic`、
      `cargo check -p nico-server --no-default-features --features traffic,etcd`、
      `cargo test -p nico-server --no-default-features --features traffic --test role_traffic`。
- [ ] `cargo deny check`（确认没有第二个 `rustls-native-certs`）。
- [ ] 文件预算：无 `.rs` 超过 500 行。

### 8. 评审加固（复查提出的三项）

- [ ] TLS fixture 上移到工作区根 `fixtures/tls/`：`nico-http`、`nico-config-etcd` 不再向上够
      `nico-server/tests` 的目录；`include_bytes` 统一为 `CARGO_MANIFEST_DIR` 相对拼写，
      约定记录在 `fixtures/tls/README.md`，布局录入 `directory-structure.md`。
- [ ] TLS 材料读取有界：校验期与构建期的每次读取都经 `nico_config::read_bounded`，共用
      `MAX_TLS_MATERIAL_BYTES`（1 MiB，bootstrap → `source_build` 一条 re-export 链）；
      bootstrap 侧与 `source_build` 侧各有超限拒绝测试。
- [ ] `env:` 口令受 `MAX_PASSWORD_BYTES` 约束：收口在两条来源共同的 `accepted`，边界值
      （恰好上限 / 上限加一）有测试。
- [ ] `endpoints.rs` 死分支清理：`split('/').next().unwrap_or("")` → `split_once`。



- 步骤 2 后：错误字段是否全部落在 `config_source.tls.*` / `config_source.auth.*`，且不含字节。
- 步骤 3 后：`nico-config-etcd` 是否仍然不认识 bootstrap 文件与测试目录（只收字节）。
- 步骤 5 后：端到端是否证明"TLS + 凭据"真的走了网络，而不是解析层自证。

## 回滚点

| 回滚到 | 方式 |
|---|---|
| 无 TLS/认证 | 回退步骤 2–5；bootstrap 删两个块即回到明文 |
| 只有 schema、不连接 | 回退步骤 3–4（校验先行，不会出现"配置被接受但不生效"） |
| 纯拆分 | 步骤 1 可单独保留（行为不变） |
