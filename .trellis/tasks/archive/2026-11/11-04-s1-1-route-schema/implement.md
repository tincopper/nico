# 路由 schema 扩展与四段校验 — 实施计划

> 顺序原则：先做**纯搬迁**（行为不变、可单独回滚），再做模型扩展，最后逐段加校验。
> 每一步结束都必须 `cargo test --workspace` 全绿，且被改文件稳定在 500 行以内。

## 0. 开工前置

- [x] 确认任务已 `task.py start`（状态 `in_progress`），分支干净。
- [x] 复跑基线：`cargo test --workspace --all-features --locked`（当前 127 个测试）、
      `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`。
- [x] 记录 `wc -l crates/gateway-core/src/*.rs` 作为拆分前基线（`config.rs` 670 行）。

## 1. 纯搬迁 A：版本标识独立

- [x] 新建 `crates/gateway-core/src/version.rs`，迁入 `Generation`、`GenerationOverflow`、
      `Revision`、`RevisionOrdering`、`ConfigDigest`（含 `Display` 与单元测试，原样搬）。
- [x] `config.rs` 删除对应定义并 `pub use crate::version::{...}`；`lib.rs` 重导出不变（对外路径不变）。
- [x] 门禁：`cargo test --workspace --all-features --locked`、`cargo clippy … -D warnings`、`cargo +nightly fmt --all -- --check`。
- [x] 行数：`version.rs` ≤ 200，`config.rs` 减到 ~420。

**评审门**：纯搬迁提交的 `git diff` 只包含移动与 `use` 调整，无逻辑变化。

**回滚点**：单独 revert 本步即可，行为与测试不受影响。

## 2. 纯搬迁 B：文档模型与错误独立

- [x] 新建 `config/document.rs`：`Document`、`Limits`、`ParseLimits` 与 accessor。
- [x] 新建 `config/error.rs`：`DocumentError` + `category()` 映射 + `Send + Sync + 'static` 断言。
- [x] `config.rs` 保留模块文档、`pub use` 与既有的最小 `parse`（暂不拆 `parse.rs`，避免同一步做两件事）。
- [x] 门禁同上；`config.rs` ≤ 200 行，`document.rs` ≤ 250，`error.rs` ≤ 200。

**评审门**：对外 `gateway_core::*` 路径与既有测试零改动即通过；`git diff` 仅搬迁。

**回滚点**：revert 本步，等价于回到单文件 `config.rs`。

## 3. 解析边界：alias、节点、深度

- [x] 新建 `config/parse.rs`：把 `Document::parse` 的边界逻辑迁入。
- [x] 实现 alias/anchor 词法拒绝：token 起始位置（行首或前置分隔符为空白 / `[` / `{` / `,` / `-`）
      后出现 `&` 或 `*` 且紧跟标识符字符 → `DocumentError::AliasForbidden`。
- [x] 用 `serde_norway::Value` 一次解析，随后**迭代式**统计节点数与最大深度（显式栈），
      超 `max_nodes` / `max_depth` → `TooManyNodes` / `TooDeep`。
- [x] 新建 `config/bounds.rs`：`SchemaBounds`（`max_routes` / `max_upstreams` / `max_providers` /
      `max_nodes` / `max_depth`，全部 `NonZeroUsize`）+ `Bound`（字段与配置键的唯一来源）+ `Default`
      （默认值唯一来源）+ `get` / `with`（唯一写入路径，拒绝 0）；
      `ParseLimits` 改为 `{ max_bytes, schema }`，`new(max_bytes)` 取 `SchemaBounds::default()`。
      `gateway-server` 本步零 diff（接线在 §8）。
- [x] 测试：`&anchor`/`*alias` 被拒；`"a*b"`、`"&&"`、注释中的 `*` 不被误判；深文档触发 `TooDeep`
      而不是栈溢出；节点数超限触发 `TooManyNodes`。
- [x] 门禁同上；`parse.rs` ≤ 300 行。

**评审门**：`gateway-server` 在本步仍零 diff；新增边界错误类别为 `Limit`。

## 4. 模型扩展：routes / upstreams / providers

- [x] `config/raw.rs`：新增 `RawProvider` / `RawUpstream` / `RawRoute` 与 `RawDocument` 字段，
      全部 `#[serde(deny_unknown_fields)]`。
- [x] `config/document.rs`：新增 `Provider`、`ProviderKind`、`Upstream`、`Scheme`、`Route`、
      `RouteTarget`，字段私有 + accessor；`Document` 增加三个 `Vec` 与 `routes()/upstreams()/providers()`。
- [x] 构造阶段把 `RawRoute.upstream/provider` 两个 `Option<String>` 转成 `RouteTarget`，
      同时给出 `RouteTargetMissing` / `RouteTargetAmbiguous`。
- [x] `lib.rs` 增加 `Provider`、`ProviderKind`、`Route`、`RouteTarget`、`Upstream` 重导出。
- [x] 测试：带两条路由（upstream 目标 + provider 目标）的示例解析成功；缺少 `routes`/`upstreams`/
      `providers` 段时解析失败（`Yaml`），不做「缺失即空」的隐式默认。
- [x] 门禁同上；`document.rs` ≤ 300，`raw.rs` ≤ 250。

**评审门**：`Document` 仍只能经 `parse` 构造；字段私有；无新增 `pub` 字段。

## 5. 引用校验

- [x] `config/reference.rs`：`DuplicateId`（route/upstream/provider 三类）、`ReferenceUnknown`、
      `RouteTargetMissing`、`RouteTargetAmbiguous` 的规则实现。
- [x] 测试：未知 upstream、未知 provider、缺目标、双目标、重复 route id、重复 upstream/provider 名。
- [x] 门禁同上；`reference.rs` ≤ 250。

## 6. 语义校验

- [x] `config/semantic.rs`：名称字符集、`routes[].host` 与 `upstreams[].host` 形态、`path_prefix` 形态、`(host, path_prefix)` 唯一、
      `timeout_ms` 区间、`max_body_bytes` ≤ 文档上限、`base_url` 前缀、`credential_ref` 前缀、
      `port != 0`。
- [x] 每类错误断言**具体变体**（`InvalidField { field, .. }` 用 `matches!` 断言字段名，
      数值类断言 `LimitOutOfRange`）。
- [x] 门禁同上；`semantic.rs` ≤ 300。

**评审门**：`path_prefix` 只做语法校验，不定义匹配后缀语义（属 `1.2`）。

## 7. 数量上限与编排

- [x] `config/validate.rs`：按「引用 → 语义 → 数量」顺序编排（数量检查放最后，保证错误优先级稳定）。
- [x] `TooMany { what, count, limit }` 覆盖 routes / upstreams / providers。
- [x] `parse.rs` 只负责边界与形状，`validate.rs` 负责四段，`Document::parse` 是唯一入口。
- [x] 测试：三类数量上限各一例，断言 `Limit` 类别。

## 8. bootstrap 接线

- [x] `gateway-server/src/bootstrap.rs`：`RawBootstrap.schema_bounds: Option<RawSchemaBounds>`
      （`#[serde(deny_unknown_fields)]`），段内字段均为 `Option<usize>`；「没写」用 `None` 表达，
      从 `SchemaBounds::default()` 起步，不复制默认数字。
- [x] 启动校验（`BootstrapError::InvalidField`）：`maximum(bound)` 一张表给出策略上限
      （`max_routes`/`max_upstreams`/`max_providers` ≤ 1_000_000，`max_nodes` ≤ 10_000_000，
      `max_depth` ≤ 128），`resolve` 遍历 `[(Bound, Option<usize>); 5]` 应用；字段名取自
      `Bound::name()`，消息里的上限取自 `maximum(bound)`。0 与越界值必须失败；未知键必须失败。
- [x] `BootstrapConfig::parse_limits(&self) -> ParseLimits`：`max_bytes` 取 `config_source.max_bytes`，
      `schema` 取合并后的 `SchemaBounds`；`gateway-server/src/lib.rs` 的 `ParseLimits::new(...)`
      改为调用它。
- [x] `config/bootstrap.yaml`：可选地写出 `schema_bounds` 段，并在注释里说明「整段或字段可省略，
      省略取默认值」；不要把默认数字再抄一份进注释。
- [x] 测试（`bootstrap.rs` 单元测试）：整段省略 → 全默认；只写 `max_routes` → 其余取默认；
      未知字段被拒；每个 `Bound` 的 0 值与「超过策略上限」各一例被拒，断言
      `BootstrapError::InvalidField` 的字段名与消息中的上限。
- [x] 测试（`gateway-server/tests/`，夹具需补必填段）：bootstrap `max_routes: 1` + 含 2 条路由的文档 → 管线拒绝，
      `degraded == true` 且 generation 不变；随后换成 1 条路由的文档 → 正常发布。
- [x] 行数：`bootstrap.rs` 361 行（校验表收敛后更短），远低于 500。
- [x] 门禁同上；`gateway-config` 无行为改动（重导出改指 `gateway_core::version`，属路径收敛）。

**评审门**：默认值只在 `SchemaBounds::default()` 一处；bootstrap 只做「文件覆盖 + 区间校验」，
不复制默认数字；reload 管线逻辑未变。集成测试夹具集中在 `tests/support/mod.rs`，三个场景文件
（`pipeline.rs` 451 行、`schema_bounds.rs` 145 行、`local_file.rs` 246 行）各自在 500 行以内。

**回滚点**：若接线导致 bootstrap 与文档上限语义分叉，回退本步、保留 `ParseLimits` 默认值，
并把接线拆成独立任务（不要在两处同时维护默认值）。

## 9. 示例、文档与属性测试

- [x] `config/resources.yaml` 增加一条 API 路由（`/api/` → `api-backend`）与一条 AI 路由
      （`/v1/chat/` → `openai-primary`），并补对应 `upstreams` / `providers` 段。
- [x] `Document::parse` 的 doc test 改为带 routes 的示例；失败示例覆盖「未知引用」。
- [x] proptest 扩展：任意字节不 panic；合法文档解析后 `routes().len() <= ParseLimits.max_routes`。
- [x] `//!` 与公开项文档补齐（`# Errors` 覆盖所有新增变体）。

## 10. 收尾

- [x] 全量基线五条：
      `cargo +nightly fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`、
      `cargo test --workspace --all-features --locked`、`cargo deny check`、
      `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [x] 文件预算：`find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'`
      不得有本任务新增的超限文件（历史遗留文件仍会打印，需确认未被本任务加长）。
- [x] 依赖边界：`cargo tree -p gateway-core | grep -E 'pingora|gateway-http|gateway-ai|gateway-config|notify|etcd'` 无输出。
- [x] `gateway-config` 零 diff；`gateway-server` 的 diff 仅限 `bootstrap.rs`（`schema_bounds` + `parse_limits()`）、
      构造 `ParseLimits` 的调用处、以及新增测试。
- [x] 按 `prd.md` 的验收标准逐条核对，未满足项说明原因。

## 11. 命令清单

```bash
# 迭代
cargo test -p gateway-core --all-features --locked
cargo test --workspace --all-features --locked

# 收尾
cargo +nightly fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo deny check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# 预算与边界
find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'
cargo tree -p gateway-core | grep -E 'pingora|gateway-http|gateway-ai|gateway-config|notify|etcd'
```

## 12. 明确不做

- 不实现路由匹配、优先级、`CompiledRoute`（`1.2`）。
- 不解析 `credential_ref`、不读取环境变量或文件、不注入任何凭证（`1.4`）。
- 不实现策略引用字段（`1.3` 按需追加）。
- 不新增 crate、不引入新依赖；`gateway-server` 只新增 `schema_bounds` 接线，不改 reload 管线语义。
- 不提交（提交由用户确认后执行；pre-commit hook 会跑 Rust 门禁）。
