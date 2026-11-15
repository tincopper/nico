# 实施：真实 etcd client + 进程内 mock server

## 第 1 步：依赖与 proto

- [x] 探针：`etcd-client` 0.14.1 可拉取；**无 `pub` feature 也无 server 类型**（proto 仅在
      pub-response-field feature 下暴露 response 类型）→ 决定 vendor 三份 proto 自己生成 server。
- [x] `proto/etcdserverpb/{rpc,kv,auth}.proto`：从 etcd-client 0.14.1 vendor，剥离
      gogo/annotations。
- [x] `Cargo.toml`：`client = ["dep:etcd-client", "dep:tonic"]`（默认开）；`etcd-client` 0.14
      `default-features = false`；tonic 0.12（codegen/transport/prost）；build-dep `tonic-build`；
      dev-deps 加 `prost`、`tokio-stream`（net+sync，mock 的 listener/Receiver 包装需要）。

## 第 2 步：生成代码

- [x] `build.rs`：`compile_protos` 三份 proto，`build_client(false).build_server(true)`。
- [x] `client/proto.rs`：`#[cfg(test)] pb::{authpb, mvccpb, etcdserverpb}` 三个 include 模块，同父
      （`etcdserverpb` 引用 `super::mvccpb` / `super::authpb`）；头部
      `#![allow(missing_docs, dead_code, clippy::all, unreachable_pub)]`——生成代码不参与手写
      lint。

**检查点 A**：`cargo check -p nico-config-etcd --all-features --all-targets` 绿。

## 第 3 步：真实 client

- [x] `client/real.rs`：`GrpcEtcdClient`（`Mutex<Client>`，方法需 `&mut`）；`classify` 把
      `GRpcStatus` 的 `Unauthenticated | PermissionDenied` → `Auth`，其余 → `Connect`；`get` →
      `KvEntry`；`watch` → inclusive `with_start_revision`，后台任务把每个响应映射为 outcome
      写入 `WATCH_CAPACITY` 通道，流结束 → `Connect`。
- [x] 三个 wire 事实记录在映射处：created 确认被 `Client::watch` 内部消费（`Established` 永不
      产生）；fragment 不请求不透传；canceled 原样透传。
- [x] `lib.rs` 导出 `GrpcEtcdClient`（`pub mod real` + re-export）。

## 第 4 步：mock server

- [x] `client/mock_server.rs`（`#[cfg(all(test, feature = "client"))]`）：`StoreState`（单 key
      完整历史 + `put`/`delete`/`compact`/`set_require_auth` 控制面 + broadcast live tail）、
      `MockKv`（range 读最新；其余 unimplemented）、`MockWatch`（创建确认 → compact 取消 →
      历史重放 → live tail；Lagged → 取消）。
- [x] 端口：`TcpListener::bind("127.0.0.1:0")` + `serve_with_incoming(TcpListenerStream)`，无
      free-port 竞态。

**检查点 B**：`cargo check` 绿。

## 第 5 步：端到端测试

- [x] `tests/e2e.rs`（12 条）：seam 层 8 + 全链 4，清单见 PRD 验收。全部一次通过。
- [x] 26/26（14 scripted + 12 e2e）绿；clippy `--all-features --all-targets` 0 条；`+nightly fmt`。

## 第 6 步：文档与收尾

- [x] `cargo tree -p nico-server --no-default-features --features traffic` / 默认构建：无
      tonic/etcd-client；`client` feature 引入 tonic 0.12.3 + hyper 1.12.0（第二套栈，`design.md`
      记录）。
- [x] README「当前限制」更新：真实 gRPC client 已接入并在进程内 mock server 上验证。
- [x] PRD 按最终事实修订（fragment 验收口径移到 adapter 层）。
- [x] 全树回归（`cargo test --workspace --all-features`：全部 suite 0 failed）、全树 clippy 0 条、
      `fmt --check` 干净；已归档（`--no-commit`）。提交由用户单独指示。

## roadmap §5 取证项的最终状态

| 取证项 | 上任务状态 | 本任务证据 |
|---|---|---|
| Watch API 形状（created/canceled/compact_revision/events/fragment） | scripted 测试 | e2e：`watch_replays_history_from_the_start_revision_inclusive` / `a_compacted_start_is_canceled_naming_the_revision`；created 在 `Client::watch` 内消费（`etcd-client` 源码 `assert!(resp.created())`）实证于本机 |
| inclusive `start_revision` 不重不漏 | scripted 测试 | e2e：重放 = 恰好 `doc-2` 一条 |
| compaction → ResyncRequired | scripted 测试 | e2e 全链：`a_compacted_start_answers_one_resync` |
| fragment → 全量重载 | scripted 测试 | **wire 上不可达**：`etcd-client` 不请求、不透传 fragment；语义覆盖保留在 adapter 层 |
| auth 失败 → Unavailable | scripted 测试 | e2e：`auth_refusal_maps_to_the_seams_auth_error`（seam）+ `an_auth_refusal_is_unavailable_at_the_contract`（全链） |
| key 删除 → Unavailable | scripted 测试 | e2e：delete 事件（seam）+ 缺失 key Unavailable（全链） |
| endpoint failover；第二套栈检查 | 随真实 client 推迟 | **栈检查完成**（tonic 0.12.3 + hyper 1.12.0，门控隔离）；**failover 仍未验证**——属 `etcd-client` 自身配置，adapter 侧折叠为 `Unavailable` 已覆盖 |
