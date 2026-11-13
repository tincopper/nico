# 设计：部署形态

## 1. 不变量与失败模式

| 不变量 | 违反时的失败模式 |
|---|---|
| **I1 role 是制品属性。** `ROLE` 在 `docker build` 时决定 feature，镜像里没有"切换 role"的开关。 | 一个 `traffic` 镜像加载了声明 provider 路由的文档，发布期拒绝、保 LKG——部署者以为是网关坏了，其实是制品与配置不匹配。 |
| **I2 runtime 不含构建工具链。** 只有二进制、CA 根证书、配置目录、可写缓存目录。 | 镜像带上 `cc` 与整个 cargo 缓存，体积以 GiB 计，且把构建期的 CVE 面带进生产。 |
| **I3 配置与缓存分离。** `/etc/nico/` 只读挂载，`/var/lib/nico/cache` 是可写卷。 | LKG 缓存写进只读挂载 → 启动失败；或写进容器可写层 → 容器重建即丢失上次好配置。 |
| **I4 非 root。** 进程以固定 uid/gid 运行，只拥有缓存目录。 | 进程能改写自己挂载进来的配置，一份被推送的恶意文档可改写 bootstrap。 |
| **I5 默认配置可独立运行。** `docker run` 不挂任何卷也能起。 | 镜像只是一个"需要额外三份文件才能跑"的半成品，清单成了唯一入口。 |
| **I6 两个 role 共用一份配置形状。** 样例文档只有 upstream 路由。 | `traffic` 镜像因样例里写了 provider 路由而在发布期拒绝，I1 的验证变成假失败。 |

## 2. 制品拓扑

```
docker build --build-arg ROLE={llm|traffic}
        │
        ├── stage builder  (rust:1.99.0-slim-bookworm + build-essential)
        │     cargo build --release -p nico-server --bin nico
        │       ROLE=llm     → 默认 feature
        │       ROLE=traffic → --no-default-features --features traffic
        │
        └── stage runtime  (debian:bookworm-slim + ca-certificates)
              /usr/local/bin/nico                 (stage 1)
              /etc/nico/{bootstrap,resources}.yaml (deploy/)
              /var/lib/nico/cache                 (owned by uid 10001)
              USER 10001:10001
              ENTRYPOINT ["nico"]
              CMD ["--bootstrap", "/etc/nico/bootstrap.yaml"]
```

`ROLE` 的合法性在 builder 阶段用一个 shell `case` 断言，非法值 `exit 1`。这不是多余检查：
静默回落到默认 feature 会造出一个自称 traffic、实则带 LLM 的镜像（I1）。

## 3. 为什么是 Debian slim 而不是 scratch / distroless

`nico` 动态链接 glibc（`x86_64-unknown-linux-gnu`），且 rustls 在上游 TLS 握手时通过
`rustls-native-certs` 读平台根证书。`scratch` 两者都缺。多阶段不是把二进制 `COPY` 进空镜像
就完事——runtime 必须带 `ca-certificates`，否则所有真实 provider 都不可达，而明文 mock 的
测试全绿会掩盖这一点。

用 `debian:bookworm-slim` 与 builder 的 `rust:1.99.0-slim-bookworm` 对齐 libc 版本，避免
"builder 上新 glibc、runtime 上旧 glibc"的链接期/运行期错配。

## 4. 配置形状

镜像内 `deploy/bootstrap.yaml` 与仓库根的 `config/bootstrap.yaml` 语义相同，四处差异都是
容器视角：

| 字段 | 仓库开发版 | 镜像版 | 理由 |
|---|---|---|---|
| `config_source.path` | `./config/resources.yaml` | `/etc/nico/resources.yaml` | 绝对路径，不依赖 WORKDIR 之外的相对布局 |
| `cache_dir` | `./.tmp/cache` | `/var/lib/nico/cache` | 指向可写卷；与只读配置分离（I3） |
| `watch` | `notify+signal` | `signal` | 容器内 `notify` 依赖宿主 inotify 事件跨挂载传播，不可靠；`signal` 是显式 reload |
| `metrics` | 缺省（关闭） | `listen: 0.0.0.0:9090` | 清单的 healthcheck 需要一个 HTTP 面 |

`resources.yaml` 样例只声明一个 upstream + 一条路由，无 provider、无凭证、无密钥。
`credential_ref` 在 README 的 compose 示例里用 `env:...` 演示注入，不写进仓库里的样例文档。

## 5. 健康检查与停机

- `nico-metrics` 定死 `/health/live`（进程可答即 200）与 `/health/ready`（首次成功发布后 200）。
  compose 的 healthcheck 打 `/health/ready`，因为它同时证明进程活着**且**有一份已发布的快照。
- `stop_grace_period: 35s` ≥ bootstrap 的 `graceful_shutdown_ms: 30000`，留 5s 余量给
  `grace_period_ms` 与容器停机的编排开销。短于它会在流式响应中途切断。
- 不需要 `init: true`：`nico-server` 内部把 SIGTERM 接到 `CancellationToken`
  （`server.rs:9`），进程自己就是 PID 1 时也能优雅退出。

## 6. CI

```yaml
strategy:
  matrix:
    role: [llm, traffic]
steps:
  - cargo +nightly fmt --all -- --check
  - cargo clippy --workspace --all-targets
  - cargo test --workspace
  - cargo tree -d
  - cargo deny check
  - docker build --build-arg ROLE=${{ matrix.role }} .
```

基线在宿主 runner 上跑一次即可（与 role 无关），镜像构建按 role 各一次（与制品有关）。
两者放在同一个 job 的矩阵里，让 `traffic` 的构建失败不能被 `llm` 的成功掩盖。

## 7. 备选方案与取舍

| 备选 | 为什么不做 |
|---|---|
| **Kubernetes Helm Chart** | 没有多副本、外部 KV、服务发现的需求；M3 会话粘性的前置条件未满足。Chart 会把未定的拓扑固化。 |
| **scratch / distroless 静态镜像** | glibc 动态链接 + `rustls-native-certs` 读平台根证书，两者 scratch 都没有（§3）。 |
| **把 `ROLE` 做成运行时环境变量** | 违反 I1：role 是编译期决定"编不编入 nico-llm"，环境变量改不了已链接的代码。 |
| **把配置打进二进制（`include_str!`）** | 违反 I3/I5：配置应能被卷覆盖，且 bootstrap 明确是进程外文件。 |
| **发布到 registry / 多架构** | 需要仓库密钥与发布策略，是运维动作；CI runner 单架构（R5）。 |
| **`ENTRYPOINT` 用 `sh -c` 包装** | 多一层 shell 会吞掉 SIGTERM 的转发，破坏 §5 的优雅停机。`exec` 形式直达 PID 1。 |

## 8. 回滚

部署资产是纯新增文件，产品代码零改动。回滚 = 删掉 `Dockerfile` / `compose.yaml` /
`.dockerignore` / `deploy/` / `.github/workflows/`，无任何数据迁移。已发布的镜像按 tag 处理，
不涉及仓库内容。

## 9. 验证

| 层 | 内容 |
|---|---|
| 构建 | `ROLE=llm` 与 `ROLE=traffic` 各 `docker build`；非法 ROLE 构造失败 |
| 制品 | `docker run --rm nico:<role> id` 显示 uid 10001；`ls /usr/local/cargo` 不存在 |
| 运行 | `docker run` 不带卷，日志出现 `runtime snapshot published`；`curl /health/ready` 200 |
| 清单 | `docker compose up -d` 后 healthcheck 变为 healthy |
| 阻断 | `traffic` 镜像挂一份含 provider 路由的文档 → 发布期拒绝，LKG 保住（记录结果） |
| 回归 | 全树测试、clippy、fmt、tree -d、deny 与改动前一致 |
