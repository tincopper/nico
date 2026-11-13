# 部署形态：容器镜像与部署清单

## Goal

让 `nico` 从"能从源码跑起来"变成"能作为一个制品部署"：一个多阶段构建的最小镜像、两种 role 的
制品、一份本地可起的清单，以及一个在远端验证同一矩阵的 CI。

不改产品代码。这是 roadmap §3 最后一项「部署形态：role 只做到可构建，没有镜像、部署清单或远端
CI」的收口。

## Background

- 二进制是 `nico-server` 的 `[[bin]] name = "nico"`，唯一参数是 `--bootstrap <path>`
  （`crates/nico-server/src/lib.rs:66`）。
- 两个 feature 决定制品的角色：`default = ["llm"]`（all-in-one）与
  `--no-default-features --features traffic`（不编入 `nico-llm`）。role 是**制品属性**，
  不是 bootstrap 里能写错的字段（Cargo.toml 的注释已定这个约束）。
- `rust-toolchain.toml` 钉 `1.99.0`，MSRV `1.88`。`cargo +nightly fmt` 的 import 分组是
  nightly-only，但**构建只要 stable**。
- `ring` 需要 C 编译器（`CONTRIBUTING.md`）。镜像的 builder 阶段要装 `cc`；runtime 阶段不需要。
- TLS 上游：未写 `tls.ca_file` 时用平台根证书（`SSL_CERT_FILE` / `SSL_CERT_DIR` 由 rustls 读取）。
  所以 runtime 镜像必须带 CA 根证书，否则所有真实 provider 都不可达。
- `config/bootstrap.yaml` 里 `cache_dir: ./.tmp/cache` 是**相对路径**且进程会写入（LKG 缓存），
  `http.listen` 是 `0.0.0.0:8080`。容器需要一个可写的工作目录，且不能把配置挂成只读后还期望缓存写成功。
- 优雅停机：`nico-server` 内部把 SIGTERM 接到 `CancellationToken`（`server.rs:9`），容器不需要
  init 进程；`grace_period_ms` / `graceful_shutdown_ms` 在 bootstrap 里。

## Requirements

### R1 镜像

- 多阶段：builder 装 `1.99.0` + `cc`，只编 `nico`；runtime 把二进制、CA 根证书、一个可写目录放进
  最小基础镜像。**不把 toolchain、源码、构建缓存带进 runtime**。
- `ROLE` build arg（`llm` | `traffic`）决定 `cargo build` 的 feature。非法值在构建期失败，
  不静默回落到默认。
- 以非 root 用户运行；二进制不 `setcap`（只绑 8080 高端口，无需特权）。
- `ENTRYPOINT ["nico"]`，`CMD ["--bootstrap", "/etc/nico/bootstrap.yaml"]`；配置通过卷挂载覆盖，
  镜像内自带一份可用的默认配置以便 `docker run` 即可起。
- 暴露 `8080`；metrics 由 bootstrap 的 `metrics.listen` 决定，不额外暴露（它是可选面）。

### R2 配置

- 镜像内 `/etc/nico/bootstrap.yaml` 与 `resources.yaml` 是**可运行样例**：
  - `http.listen: 0.0.0.0:8080`
  - `cache_dir: /var/lib/nico/cache`（绝对路径，指向可写目录）
  - `config_source.path: /etc/nico/resources.yaml`
  - `watch: signal`（容器内没有文件 watch 的语义，`notify` 依赖宿主事件，signal 更适合 reload）
  - `metrics.listen: 0.0.0.0:9090`（可选面打开，便于清单里 healthcheck）
- 样例里不写任何密钥；`credential_ref` 用 `env:...` 并在 compose 里演示如何注入。
- `resources.yaml` 的样例**只含 upstream 路由**（不声明 provider 路由），这样 `traffic` role
  也能直接跑——镜像的 `ROLE` 与配置内容不互相矛盾。

### R3 本地清单

- `compose.yaml`：一个服务，挂载 `./deploy/bootstrap.yaml` 与 `./deploy/resources.yaml` 到
  `/etc/nico/`（只读），一个命名卷挂到 `/var/lib/nico/cache`，端口映射，healthcheck 打
  `/health/ready`，`stop_grace_period` 不小于 bootstrap 的 `graceful_shutdown_ms`。
- `.dockerignore`：排除 `target/`、`.git/`、`.trellis/` 与所有非构建必需文件，避免构建上下文
  把整个 workspace 送进 daemon。

### R4 远端 CI

- workflow 对 `ROLE=llm` 与 `ROLE=traffic` 两个值各构建一次镜像（或等价地各跑一次
  `cargo build --no-default-features --features traffic` 与默认构建）。
- 同时跑五条基线（见 `CONTRIBUTING.md` / `scripts/gates/pre-push.sh`）：fmt、clippy、test、
  tree -d、deny。CI 不在容器里跑测试，测试在宿主 runner 上跑，镜像构建只验证制品。
- 触发：push 到 main 与 PR。

### R5 明确不做

- **不做 Kubernetes Helm Chart / Operator**：没有多副本、外部 KV、服务发现的需求（M3 会话粘性
  的前置条件都未满足），一个 Chart 现在只会把未定的部署拓扑固化。compose 是本地可验证的最小形态。
- **不做镜像发布到 registry**：需要仓库密钥与发布策略，属于运维动作，不是仓库内容。
- **不做多架构构建**：CI runner 单架构；`ROLE` 矩阵已经覆盖制品差异，跨平台留到有需求时。
- **不改产品代码**：R1–R4 全部是 `deploy/`、根级 Dockerfile/compose/CI 与文档。

## Acceptance Criteria

- [ ] `docker build --build-arg ROLE=llm -t nico:llm .` 与 `ROLE=traffic` 都成功；非法 ROLE 失败。
- [ ] 两个镜像都不包含 `/usr/local/cargo` 或源码；以非 root 用户启动。
- [ ] `docker run` 不带任何卷即可起（用镜像内默认配置），带卷时配置被覆盖。
- [ ] compose 起得来，`/health/ready` 健康检查通过，日志里有一次
      `runtime snapshot published`。
- [ ] `traffic` 镜像遇到声明 provider 路由的文档时，发布期拒绝（与 Cargo.toml 注释一致），
      有断言或手工验证记录。
- [ ] CI 矩阵两个 role 都绿，五条基线都跑。
- [ ] 全树测试与产品代码零改动；`cargo tree -d` 无新增重复栈；新增文件不违反 ≤500 行预算
      （`.rs` 文件不变，YAML/Dockerfile 不计）。

## Out of Scope

- Helm / Operator / 多副本部署、registry 发布、多架构镜像（R5）。
- 密钥管理、虚拟 key、租户（governance，已在 model-lb R10 排除）。
- 任何 Rust 代码改动。

## Open Questions

（无。`nico-metrics/src/response.rs` 定死 `/health/live` 与 `/health/ready`，已核实。）
