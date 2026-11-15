# etcd 配置源：接口先行，真实边界用 mock

## Goal

实现 etcd `ConfigSource` 适配器的**完整语义**（roadmap §5 的七条取证项），但真实的 etcd 环境——
客户端连接、Watch 网络流、auth——以内部客户端 trait 的 **mock 形式**提供。适配器逻辑是真实现，
可全部离线测试；真实 `etcd-client` 依赖**不引入**，等有可连的环境再把它接到同一个 trait 上。

这是 roadmap §5 的收口方式：七条取证项逐条变成可断言的测试，而不是留作"需要一个环境才能开始"。

## Background

- `ConfigSource` 契约（`nico-config/src/source.rs:239`）：`name()` / `load() -> ConfigVersion` /
  `watch(from) -> ConfigWatch`。`ConfigVersion { revision, bytes }`，bytes 原样传递，digest 由
  pipeline 算。
- `ConfigEvent`（`source.rs:61`）：`Snapshot(ConfigVersion)` | `ResyncRequired`。**没有 patch**：
  每个变更都是完整文档。
- `Revision`：`nico_core::Revision::comparable(seq)`，源内可比——etcd 的 `mod_revision` 直接映射。
- watch 的既有模式（`nico-config/src/fake.rs`、`local.rs`）：后台任务 + 有界 mpsc +
  `SourceWatch`（drop 取消 token）。消费者落后时**折叠成一次 `ResyncRequired`**，不缓存历史。
- spec 归属：配置中心客户端是自己的 crate + feature（`nico-config-*`）。
- `nico-config/src/local.rs:173` 已用 `Revision::comparable` 造本地序号。

## Requirements

### R1 内部客户端接缝（mock 的形状）

- 新 crate `nico-config-etcd` 定义一个**内部** `EtcdClient` trait：`get(key)` 与
  `watch(key, start_revision)`，以及 etcd watch 协议的**类型化模型**：
  `Established` / `Events(Put{value, mod_revision} | Delete)` / `Canceled { compact_revision }` /
  `Fragmented`。
- 这就是"真实环境"的边界：真实 `etcd-client` 实现这个 trait（未来工作），测试用 script 化的
  mock 实现。适配器**不认识**网络。
- 不引入 `etcd-client` / `tonic` / `hyper`：`cargo tree` 不出现第二套传输栈。

### R2 load 语义

- `load` → `client.get(key)`：`Some(entry)` → `ConfigVersion(comparable(entry.mod_revision), value)`。
- `None`（key 不存在）→ `Unavailable`，**不是**空文档。
- 文档超过 `max_bytes` → `TooLarge { size, limit }`。
- 非法 UTF-8 → `NotUtf8`。

### R3 watch 语义（roadmap §5 逐条）

1. **inclusive start_revision**：`load` 得 R，`watch(key, R+1)`；`mod_revision <= R` 的事件丢弃，
   `> R` 的恰好一次（不重不漏）。
2. **compaction**：`Canceled { compact_revision }` → 事件流上发出 `ResyncRequired` 并结束。
3. **fragment**：`Fragmented` → `ResyncRequired`（明确拒绝增量，整份重载）。
4. **key 删除**：`Delete` → `Unavailable` 错误事件（不是空配置）。
5. **auth 失败**：client 返回错误 → `Unavailable`。
6. **max_bytes**：事件携带的 value 超预算 → `TooLarge`。
7. **endpoint failover**：**是 client 的职责**（真实客户端持有 endpoint 列表）。适配器把一切
   连接类失败都折叠成 `Unavailable`，由 pipeline 重载——适配器不做重连循环。

### R4 消费者落后

- 有界通道满 → 折叠成一次 `ResyncRequired`（与 `fake.rs` 同一规则），适配器不缓存无限历史。

### R5 明确不做

- 不引入 `etcd-client` / `tonic` / `hyper` / 任何新外部依赖。
- 不实现重连循环、endpoint 列表、TLS 配置——那些属于未来的真实 `EtcdClient` 实现。
- 不做 Nacos。
- 不改 `nico-config` 既有契约；不改 pipeline。

## Acceptance Criteria

- [ ] 新 crate `nico-config-etcd`，零新外部依赖；`cargo tree -d` 无变化。
- [ ] `EtcdClient` trait + 类型化 watch 协议模型齐备；适配器不认识任何网络类型。
- [ ] R2 四条 load 语义各有测试。
- [ ] R3 七条 watch 语义各有测试（mock 按 script 驱动）。
- [ ] R4 折叠语义有测试。
- [ ] `nico-server` 增加 `etcd` feature（默认关）仅使 crate 可被未来接线；traffic 角色不受影响。
- [ ] 全树 ≤500 行 / crate、clippy 零告警、五条基线绿。

## Out of Scope

- 真实 `etcd-client` 集成、TLS/auth 凭据的具体读取、endpoint failover 的实现。
- Nacos、其它配置中心。
- pipeline 或 `nico-config` 契约的变更。

## Open Questions

（无。接缝、语义与错误映射全部由既有契约与 roadmap §5 决定。）
