# 实施：etcd 配置源（接口先行，边界 mock）

## 第 1 步：crate 骨架

- [x] `crates/nico-config-etcd/`：`Cargo.toml`（依赖 `nico-config`、`nico-core`、`async-trait`、
      `bytes`、`thiserror`、`tokio`、`tokio-util`、`futures-core`，零新外部依赖）、
      `#![deny(missing_docs)]`、workspace 成员（`crates/*` 通配自动纳入）。
- [x] `nico-server` 加 `etcd = ["dep:nico-config-etcd"]` feature（默认关）。
- [x] `nico-config-etcd` 的 `Cargo.toml` 加 `[lints] workspace = true`。

**检查点 A**：`cargo check -p nico-config-etcd` 绿；`cargo tree -p nico-config-etcd` 只出现既有依赖。

## 第 2 步：客户端接缝与类型化协议

- [x] `client.rs`：`EtcdClient` trait（`get` / `watch`）、`KvEntry`、`EtcdError`（连接/auth/超时）、
      `WatchOutcome`（Established / Events / Canceled / Fragmented）、`WatchEvent`（Put / Delete）、
      `WatchOutcomes`（`Stream<Item = Result<WatchOutcome, EtcdError>>`）。
- [x] `source.rs`：`EtcdConfigSource::new(key, max_bytes, Arc<dyn EtcdClient>)` 实现 `ConfigSource`。

**检查点 B**：`cargo check -p nico-config-etcd` 绿；适配器无任何网络类型。

## 第 3 步：load 语义

- [x] `load`：命中 → `ConfigVersion::new(Revision::comparable(mod_revision), value)`；
      key 不存在 → `Unavailable`；超 `max_bytes` → `TooLarge`；非 UTF-8 → `NotUtf8`。

**检查点 C**：load 四条测试绿。

## 第 4 步：watch 语义

- [x] 后台任务：`client.watch(key, from+1)` → 逐 outcome 映射（design §4 的表）。
- [x] inclusive 过滤：`mod_revision <= from` 丢弃；`last` 单调推进。
- [x] Canceled / Fragmented → `ResyncRequired` 并结束。
- [x] Delete → `Unavailable` 错误事件。
- [x] 事件 value 超 `max_bytes` / 非 UTF-8 → 对应错误事件。
- [x] 通道满 → 折叠 `ResyncRequired`（与 `fake.rs` 同规则）。

**检查点 D**：R3 的七条语义测试 + R4 折叠测试全绿（mock script 驱动）。

## 第 5 步：回归与文档

- [x] 全树测试、clippy、fmt、`cargo tree -d`、traffic 角色 + `--no-default-features` 编译。
- [x] `README.md` 配置源小节补 etcd 的位置（接口已实现、真实客户端待接）。
- [x] roadmap `11-03-unified-gateway-analysis/implement.md` §5 的取证项逐条标注为
      "已实现（mock 驱动测试）；真实客户端接入后按同一测试复核"。

## 验证命令

```bash
cargo test -p nico-config-etcd
cargo tree -p nico-config-etcd
cargo tree -d
cargo test --workspace
cargo clippy --workspace --all-targets
cargo check -p nico-server --no-default-features --features traffic
```

## 落地记录

| roadmap §5 取证项 | 证据 |
|---|---|
| Watch API 形状（created/canceled/compact_revision/events/fragment） | `client.rs` 的 `WatchOutcome` 四变体 + `a_compacted_watch_is_a_resync` / `a_fragmented_watch_is_a_resync` |
| 包含式 `start_revision` 不重不漏 | `the_client_is_asked_for_the_right_key_and_start` + `events_at_or_before_the_start_are_dropped` |
| compaction / watcher 落后 → ResyncRequired | `a_compacted_watch_is_a_resync` |
| fragment → 全量重载 | `a_fragmented_watch_is_a_resync` |
| auth 失败与 key 删除 → Unavailable | `an_auth_refusal_is_unavailable` / `a_deleted_key_is_unavailable_not_empty` |
| 单 key 大小上限 vs `max_bytes` | `an_oversized_document_is_too_large` / `an_oversized_event_is_too_large` |
| endpoint failover；第二套栈检查 | **随真实 client 推迟**：adapter 把连接失败折叠为 Unavailable（`an_ended_stream_is_unavailable`），failover 在接缝之下；`cargo tree -d` 在引入 `etcd-client` 时执行 |

新增语义：消费者落后 → 折叠为一条 `ResyncRequired`（与 `fake.rs` 同规则），`a_consumer_that_falls_behind_gets_one_resync`。

**本机未验证**：真实 etcd 环境不存在，mock 即"真实环境"的替身。真实 client 接入时须重跑本清单。

**行为收紧**：此前的阻塞式 send 会无限等待慢消费者；现在是 try_send + 折叠，与 fake 一致。
