# 实施：部署形态

## 第 1 步：镜像构建文件

- [x] `Dockerfile`：两阶段，`ARG ROLE=llm`，`case "$ROLE" in llm|traffic) ;; *) exit 1 ;; esac`。
      builder 装 `build-essential`（`ring` 需要 `cc`），`cargo build --release --locked -p nico-server --bin nico`，
      按 role 选 feature。runtime 用 `debian:bookworm-slim` + `ca-certificates`，
      `useradd` 固定 uid/gid 10001，`COPY --from=builder` 二进制。
- [x] `.dockerignore`：`target/`、`.git/`、`.trellis/`、`.claude/`、所有 `*.md` 之外的非构建文件，
      保留 `Cargo.toml` / `Cargo.lock` / `rust-toolchain.toml` / `crates/` / `deploy/`。

**检查点 A**：`docker build --build-arg ROLE=llm -t nico:llm .` 成功；
`ROLE=typo` 失败且错误信息指出合法值。

## 第 2 步：镜像内配置

- [x] `deploy/bootstrap.yaml`：按 design §4 的四处差异写；`metrics.listen: 0.0.0.0:9090`。
- [x] `deploy/resources.yaml`：一个 upstream（`127.0.0.1:9`，故意不可达，用于验证
      "进程能起但上游连不上"不是部署失败）+ 一条路由；无 provider、无凭证。
- [x] 两份文件 `COPY` 到镜像 `/etc/nico/`，属主 root、模式 0444（配置只读，I3/I4）。

**检查点 B**：`docker run --rm nico:llm` 起得来，日志有 `runtime snapshot published`；
`docker run --rm nico:traffic` 同样起得来（I6）。

## 第 3 步：本地清单

- [x] `compose.yaml`：单服务，镜像 `nico:llm`（`build:` 段带 `args: ROLE: llm`），
      只读挂载 `./deploy` 到 `/etc/nico`，命名卷到 `/var/lib/nico/cache`，
      端口 `8080:8080` 与 `9090:9090`，healthcheck 打
      `http://127.0.0.1:9090/health/ready`，`stop_grace_period: 35s`，`init: false`。
- [x] healthcheck 用镜像内的工具；若 slim 镜像没有 curl，用 `wget`（busybox）或在 runtime 装
      `curl`——优先选 `CMD-SHELL` + `wget -qO-`，避免为探针多装一个包。

**检查点 C**：`docker compose up -d` 后 `docker compose ps` 显示 healthy。

## 第 4 步：CI

- [x] `.github/workflows/ci.yml`：push main + PR 触发；`matrix.role: [llm, traffic]`。
- [x] 步骤：checkout → `rustup` 按 `rust-toolchain.toml` → 五条基线（fmt/clippy/test/tree -d/deny）
      → `docker build --build-arg ROLE=${{ matrix.role }} .`。
- [x] `cargo-deny` 用 `taiki-e/install-action@cargo-deny` 或等价固定版本方式安装，不 `cargo install`
      每次编译。

**检查点 D**：workflow 语法有效（本地 `actionlint` 或 GitHub 校验）；两个 role 的构建命令
在本机手工跑通。

## 第 5 步：文档与验证

- [x] README「快速开始」加"容器部署"小节：`docker build` 两种 ROLE、`docker compose up`、
      挂自己的配置、metrics 与 health 路径、`traffic` role 的配置限制。
- [x] `CONTRIBUTING.md` 的 Prerequisites 加 Docker（仅部署验证需要）。
- [x] 记录实际验证结果：若本机无 Docker，明确写"未在本机构建，CI 矩阵是唯一验证"，
      不假装跑过。

**检查点 E**：全树测试/clippy/fmt 与改动前一致（产品代码零改动）；
`git diff --stat` 里 `crates/**/*.rs` 为空。

## 验证命令

```bash
docker build --build-arg ROLE=llm -t nico:llm .
docker build --build-arg ROLE=traffic -t nico:traffic .
docker build --build-arg ROLE=typo -t nico:bad .   # 必须失败
docker run --rm nico:llm id
docker compose up -d && docker compose ps
```

## 落地记录与验证边界

| 项 | 结果 |
|---|---|
| `Dockerfile` / `.dockerignore` / `deploy/*.yaml` / `compose.yaml` / `.github/workflows/ci.yml` | 已交付 |
| 配置静态验证 | 新增 `crates/nico-server/tests/deploy_config.rs`（3 项）：shipped bootstrap 能被 `BootstrapConfig::load` 解析、resources 能被 `Document::parse` 解析且无 provider 路由、bootstrap 指向的文件名就是 shipped 文档 |
| `docker compose config --quiet` | VALID |
| 四份 YAML 语法 | `ruby -ryaml` 全部 OK |
| CI 结构 | jobs `baseline` + `image`，matrix `["llm","traffic"]` |
| Dockerfile 与仓库事实交叉核对 | toolchain `1.99.0`、二进制名 `nico`、`Cargo.lock` 存在、`deploy/` 两份文件齐 |
| **镜像构建与 smoke test** | **本机未执行**：`docker --version` 与 `docker compose version` 可用，但 daemon 未运行（`Cannot connect to the Docker daemon`）。CI 的 `image` job 是这两个 role 的唯一构建验证，必须等远端跑过才能说"镜像可用" |

**未验证项的诚实边界**：`ROLE=typo` 的构建期拒绝、非 root uid、runtime 不含 toolchain、
`docker run` 无卷即可起、`/health/ready` 返回 200、`traffic` 镜像拒 provider 路由——这些是 CI
`image` job 的断言，本会话只做了静态推演，没有在本机跑过。不要把它们当作已验证。

产品代码零改动：`git status` 里 `crates/**/*.rs` 只有新增的 `deploy_config.rs` 测试文件。
