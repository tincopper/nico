# 统一产品命名：nico-* crates 与 nico 二进制

## Goal

产品名是 **nico**，但当前的标识符前缀是品类词 `gateway`。本任务把产品名落到所有**活**代码与**活**文档上：
crate 目录/包名、Rust 导入路径、二进制名、spec 的命名契约、构建脚本与活任务文档。

命名规则随之明确为：**前缀 = 拥有者（nico），后缀 = 角色（core/config/http/server），二进制 = 产品名（nico）**。

## Requirements

### R1 映射（唯一真值）

| 现在 | 之后 | 说明 |
|---|---|---|
| `crates/gateway-core`（包 `gateway-core`） | `crates/nico-core`（包 `nico-core`） | Rust 路径 `gateway_core::` → `nico_core::` |
| `crates/gateway-config` | `crates/nico-config` | 同上，`gateway_config::` → `nico_config::` |
| `crates/gateway-http` | `crates/nico-http` | `gateway_http::` → `nico_http::` |
| `crates/gateway-server`（包 `gateway-server`） | `crates/nico-server`（包 `nico-server`） | `gateway_server::` → `nico_server::` |
| 二进制 `gateway-server` | 二进制 **`nico`** | `[[bin]] name = "nico"`；包名保持角色名 `nico-server` |
| 未建 crate `gateway-ai` / `gateway-plugins` / `gateway-config-etcd` / `gateway-config-nacos` | `nico-ai` / `nico-plugins` / `nico-config-etcd` / `nico-config-nacos` | 只改文档与计划，不建 crate |

### R2 范围

**要改（活资产）**

- `crates/**`：目录名、`Cargo.toml`（包名、`[dependencies]`、`description`）、`use` 路径、文档内链接与示例。
- 根 `Cargo.toml`：`[workspace.dependencies]` 的键与 `path`。
- `scripts/gates/pre-commit.sh`、`CONTRIBUTING.md`：命令与注释里的 crate 名。
- `.trellis/spec/**`：命名契约表、目录树、依赖图、验证命令、示例。
- `.trellis/tasks/11-03-*`、`11-04-*`（活任务）：`prd.md` / `design.md` / `implement.md` / `task.json` 的 `title`/`description` 里的 crate 名。
- `poc/FINDINGS.md`、`poc/README.md`：结论里引用的 crate 名。

**不改（历史）**

- `.trellis/tasks/archive/**`：已归档任务的记录属于历史，改名会让「当时做了什么」失真。
- `.trellis/workspace/**`：journal 记录同理。
- `.tmp/**`、`.neeko/**`：gitignored 本地产物。
- 任务目录 slug：`11-04-s1-3-gateway-plugins` / `11-04-s1-4-gateway-ai-passthrough` 是记录标识，改目录会破坏父子链接与归档引用；只改其 `prd.md` 正文与 `task.json` 的 `title`/`description`。
- 领域用词 "gateway"（HTTP 网关、AI 网关、the gateway's behaviour）保持不变：它描述这个产品**做什么**，不是产品名。

### R3 二进制

- `crates/nico-server/Cargo.toml` 增加 `[[bin]] name = "nico" path = "src/main.rs"`，安装后用户敲 `nico --bootstrap <path>`。
- `USAGE` 常量改为 `usage: nico --bootstrap <path>`；所有把它当"进程/二进制"引用的文档同步（当 crate 引用时写 `nico-server`）。

## Acceptance Criteria

- [x] `cargo metadata` 里四个包名分别是 `nico-core` / `nico-config` / `nico-http` / `nico-server`，且 `nico-server` 只有一个二进制目标 `nico`。
- [x] `rg -uu "gateway-(core|config|http|server|ai|plugins)|gateway_(core|config|http|server)"` 在 `crates/`、`Cargo.toml`、`scripts/`、`CONTRIBUTING.md`、`.trellis/spec/`、`.trellis/tasks/11-03-*`、`.trellis/tasks/11-04-*`、`poc/` 下**无命中**（归档与 journal 除外）。
- [x] `cargo run -q -p nico-server -- --help` 打印 `usage: nico --bootstrap <path>`。
- [x] 五条基线全绿：fmt、`clippy -D warnings`、`cargo test --workspace --all-features`、`cargo deny check`、`RUSTDOCFLAGS=-D warnings cargo doc`。
- [x] Feature 矩阵与依赖边界用新名验证：`cargo tree -p nico-core` 无 pingora；`cargo tree -p nico-http` 无 `nico-ai`。
- [x] 文件预算不变：无新增超 500 行文件。
- [x] `.trellis/spec/backend/directory-structure.md` 的命名表与目录树已是最终形态（`nico-<domain>`），没有残留旧名或"修订记录"段落。

## Out of Scope

- 目录 slug 重命名、归档与 journal 的历史改写。
- 任何行为变更（本任务只改名字；测试数量与语义不变）。
- `.github/`、CI、部署清单：本仓库当前没有这些文件，出现时再随新名编写。
