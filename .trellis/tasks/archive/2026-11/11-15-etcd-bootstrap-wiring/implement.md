# 实施：bootstrap `type: etcd` 接线

## 第 1 步：mock server 暴露为 `mock-server` feature

- [x] `nico-config-etcd/Cargo.toml`：`mock-server = ["client", "dep:prost", "dep:tokio-stream"]`；
      `prost`、`tokio-stream` 增加 optional regular 依赖（与 dev-deps 同版本，cargo 归一）。
- [x] `proto.rs` / `client.rs` / `lib.rs`：门控 `cfg(any(test, feature = "mock-server"))`，类型
      `pub`（`MockServer`、`StoreState` 及控制面方法），带文档。
- [x] `nico-server` dev-dependencies：`nico-config-etcd = { features = ["mock-server"] }`。

**检查点 A**：`cargo check -p nico-config-etcd --features mock-server` 绿且不破坏默认构建。

## 第 2 步：failover 取证（nico-config-etcd e2e）

- [x] `tests/e2e.rs` 增一条：连接列表含一个不可达 endpoint，connect 成功、get/watch 正常。
      （探针已完成，测试化并注明结论：failover 在 `etcd-client` 之下成立；client runtime 必须
      存活——后者写入 design.md 与构造实现。）

## 第 3 步：bootstrap schema 与验证

- [x] `bootstrap/source.rs`（新子模块）：两段式解析（`config_source: Value` → 按 `type` 分派到
      `RawLocalSource` / `RawEtcdSource`，各自 `deny_unknown_fields`）；etcd 变体验证
      endpoints 非空 + URL 合法 + key 非空。
- [x] `SourceSelection` 枚举进 `BootstrapConfig`；`WatchMode` 移入 Local 变体；
      `local_config()` → `source()`；访问者更新（`server.rs`、`lib.rs`、测试）。
- [x] 无 `etcd` feature 时：`type: etcd` → `UnsupportedSource`，文本列出支持类型。

## 第 4 步：构造

- [x] `build_source(&BootstrapConfig) -> Result<Arc<dyn ConfigSource>, SourceBuildError>`：
      local 同步构造；etcd 走守护线程（current-thread runtime + `pending()` 常驻）connect。
- [x] `lib.rs` main 调 `build_source`，错误路径打印并退出。

**检查点 B**：`cargo check -p nico-server`（默认）与 `--features etcd` 均绿，0 警告。

## 第 5 步：测试

- [x] bootstrap 测试矩阵（有/无 feature：45 / 54 测试绿）。
- [x] `build_source` e2e（`tests/etcd_bootstrap.rs`，2 条）：bootstrap 文件 `type: etcd` → mock server → `source.load()` 出文档；全 endpoint 不可达 → 构造成功（connect 惰性）、load 报 Unavailable。
- [x] 连接失败路径（探针修订）：connect 是惰性的，不可达在首个操作浮现为 `Unavailable`——启动不阻塞于 etcd 可达性，失败走管线重载循环。

## 第 6 步：收尾

- [x] `cargo tree` 栈纪律复核（`--edges normal`：默认与 traffic 均 0；tonic 0.12.3 + hyper 1.12.0 仅经 `etcd` feature / dev-deps 的协议替身）（默认、traffic 无 tonic；etcd feature 引入）。
- [x] README 限制段更新（`type: etcd` 可用；TLS/认证凭据未暴露）。
- [x] roadmap §5 failover 项闭合（见下表）。
- [x] 全树回归（`cargo test --workspace --all-features` exit 0，无失败）、全树 clippy 0 条、`fmt --check` 干净；已归档（`--no-commit`）。提交由用户指示。

## 探针记录（两个决定设计的结论）

| 问题 | 结论 | 证据 |
|---|---|---|
| 含死 endpoint 的连接列表 | **failover 成立**：connect 成功，get/watch 正常（`etcd-client` 在接缝之下自行平衡/转移） | e2e `a_dead_endpoint_in_the_list_does_not_break_the_client` |
| connect 的 runtime 被丢弃 | **client 死亡**：transport 任务随 runtime 消亡，之后每个操作 `transport error`；runtime 存活时操作可从任意 runtime 驱动 | 探针一（失败）/ 探针二（守护线程形状，通过）→ `source_build.rs` 的守护线程 |
| 全部 endpoint 不可达时 connect | **惰性成功**：不阻塞启动，不可达在首个操作浮现为 `Connect`，adapter 折叠为 `Unavailable` | 探针三 → e2e `an_etcd_bootstrap_with_every_endpoint_dead_still_builds_and_reports_unavailable` |

## roadmap §5 的最终状态（etcd 里程碑收口）

| 取证项 | 状态 | 证据 |
|---|---|---|
| endpoint failover | **闭合** | 上表第 1 行：含不可达 endpoint 的列表 connect 成功、操作正常；多 endpoint 原样穿过 bootstrap → client |
| 第二套栈检查 | **闭合**（11-15-etcd-real-client 完成栈面，本任务复核） | `--edges normal`：默认与 traffic 角色均无 tonic/hyper；`etcd` feature 才引入 |
| bootstrap 接线 `type: etcd` | **交付**（本任务） | bootstrap schema + 验证矩阵 + `build_source` 整链 e2e |
