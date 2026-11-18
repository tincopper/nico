# 实施计划：bootstrap 配置 nginx 对齐

> 阶段以 crate 为单位划分，保证每阶段结束时 workspace 可编译；检查点是真实门禁，不是走形式。

## 阶段 1：`nico-http` 类型面与数据面

- [x] `server_settings.rs`（新）：`ServerSettings` + `new()` 默认值 + `resolve` 校验（threads `1..=256`、grace/shutdown `1..=300_000`、shutdown ≥ grace）+ `ServerSettingsError`（消息引用常量）；单元测试。
- [x] `settings.rs` 重造：`Listener` 枚举（Plain/Tls）、`HttpSettings { listeners, h2c, tls }`、`resolve(listeners, h2c, tls)`（非空、地址两两不同、h2c 无明文拒绝）、`plain_addresses()`；`HttpSettingsError` 字段族收窄；删除 `listen()`/`inbound_tls()`。
- [x] `service.rs`：`server_conf(&ServerSettings, &TlsPolicy)`；`service()` 遍历 listeners 注册（Plain → add_tcp，Tls → 复检 + static resolver + `add_tls_with_settings`）；`options.h2c = settings.h2c()`。
- [x] `proxy.rs`：删除 `inbound_tls` / `set_inbound_tls` / `inbound_tls()`。
- [x] `hooks/request.rs`：`x-forwarded-proto` 改按 `session.digest().ssl_digest` 判定；单测构造 `SslDigest` 覆盖两分支 + 客户端已提供值不覆盖。
- [x] 既有测试迁移：settings/service 的旧断言改写（行为平移、字段路径更新）；`inbound.rs` 不动。

**检查点 A**：`cargo test -p nico-http --all-features` + `cargo clippy -p nico-http --all-targets --all-features -- -D warnings` 全绿。

## 阶段 2：`nico-server` bootstrap

- [x] `bootstrap/runtime.rs`（新）：`RawRuntime` + `resolve` → `ServerSettings`。
- [x] `bootstrap/proxy.rs`（新）：`RawProxy { tls: Option<RawTls> }` 封装；顶层 `tls` 字段改名 `proxy`，字段路径 `tls.ca_file` → `proxy.tls.ca_file`（语义原样：缺席 = 平台根、空段 = 错误、ca_file 替换平台根）；`bootstrap/tls.rs` 仅改字段路径字符串。
- [x] `bootstrap/source.rs`：`RawConfigSource` 归组五个 reload 旋钮（debounce_ms/max_wait_ms/cache_dir/allow_bootstrap_cache/schema_bounds 从 RawBootstrap 迁入）；`BootstrapConfig` 内部字段与访问器不变，顶层裸字段成为未知字段。
- [x] `bootstrap.rs`：`runtime: Option<RawRuntime>`（None → `ServerSettings::new()`）；`http: RawHttp` 改必填；新增 `BootstrapConfig::runtime()`；`serve_with` 适配（`server_conf(runtime, tls)`）。
- [x] `bootstrap/http.rs`：`RawHttp { h2c, listeners }`（顶层 `listen` 字段删除）；`RawListener { listen, tls: Option<RawTlsIdentity> }`。
- [x] `bootstrap/http/tls.rs` → `bootstrap/http/listener.rs`：条目解析 + 证书校验，字段路径 `http.listeners[N].tls.cert_file`/`key_file`。
- [x] `bootstrap/metrics.rs`：`resolve(&self, listeners: &[&str])`，碰撞对象改地址集合。
- [x] `bootstrap/tests/http_tls.rs` 重写：runtime 段矩阵（缺省/越界）、listeners 场景（明文/TLS/双条目/空列表/地址重复/旧字段拒绝）、证书坏路径（新字段路径）、h2c 矩阵（无明文拒绝、与 TLS 条目并存通过）、metrics 集合碰撞、proxy 段矩阵（缺席/空段/ca_file 替换、旧顶层 `tls` 被拒）、config_source 归组矩阵（新路径生效、旧顶层裸字段被拒）。
- [x] `tests/support/http.rs`：bootstrap 写入 helper 更新为 listeners 形态。

**检查点 B**：`cargo test -p nico-server --all-features` + clippy 全绿。

## 阶段 3：e2e 与文档

- [x] `tests/tls_inbound.rs`：TLS-only 场景改造（listeners 单 TLS 条目）+ 双 listener 场景新增（明文口 `http` / TLS 口 `https` / 交叉连接无响应）+ ALPN h2 保留。
- [x] 既有明文 e2e 零改动回归确认。
- [x] `config/bootstrap.yaml`：`runtime` + `http.listeners` 示例重写。
- [x] `README.md`：当前限制（TLS 部分）更新为 listeners 形态。
- [x] spec 三件：`pingora-guidelines.md`（§2 签名 server_conf/service、§3 契约表 runtime/listeners/proxy.tls、Upstream TLS 表与 Inbound TLS facts 表、§4 错误矩阵、§6 测试表）、`error-handling.md`（Inbound TLS startup errors 字段路径、ensure_usable 引用）、`directory-structure.md`（bootstrap 树：runtime.rs、proxy.rs、http/listener.rs；nico-http 树：server_settings.rs）。
- [x] `research/option-b-analysis.md` 标注 superseded（已被 nginx 分析 + 本设计取代的部分）。

**检查点 C**：完整门禁
```bash
cargo +nightly fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features
cargo check --workspace --all-features
cargo check --workspace --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'
cargo tree -p nico-http --prefix none | grep nico-llm || true
```

## 阶段 4：收尾

- [x] PRD 验收勾选、implement.md 检查点确认。
- [x] 不提交（除非用户明确要求）；报告真实门禁输出与已知缺口。

## 回滚点

- 阶段 1 独立回退：`nico-http` 改动可整体 revert（编译会断 nico-server——回退时按 commit 边界）。
- 阶段 2 独立回退：bootstrap 改动可 revert。
- 阶段 3 纯测试/文档，可独立回退。

## 上下文清单

- 已挂：prd.md、design.md、spec 三件（pingora-guidelines / error-handling / directory-structure / index / quality-guidelines）、research 两份。
- 实现时按需补：`11-17-inbound-tls` 已归档至 `archive/2026-11/`（InboundTls/resolver/e2e fixture 的来源）。
