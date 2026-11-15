# 设计：真实 etcd client + 进程内 mock server

## 总览

`EtcdClient` 接缝（`get` / `watch`）与其上的 adapter 语义已在 `11-14-etcd-config-source` 落地，
当时接缝之下是 scripted client。本任务把接缝填满：一侧是真实的 gRPC client（`etcd-client`
crate），另一侧是进程内 etcd v3 mock server——"需要真实 etcd 环境"的验证项由后者承接，整条链
（adapter → 接缝 → gRPC wire → 协议替身）在测试中完整组合。

分层不变量不变：**策略与网络分离**。`real.rs` 只做两个词表之间的映射（etcd wire ↔ 接缝协议），
过滤、resync 折叠、大小预算都在 adapter（`source.rs`），mock server 只生产协议形状。

## 真实 client（`client/real.rs`）

- `GrpcEtcdClient { client: Mutex<Client> }`：`etcd_client::Client` 的方法取 `&mut self`（多路复用
  单连接），锁只在发起调用时持有，不跨调用方可见的 `.await`。
- **错误分类**：`Error::GRpcStatus(status)` 里 `Unauthenticated | PermissionDenied → EtcdError::Auth`，
  其余一切（传输层、超时、流结束）→ `EtcdError::Connect`。分类只看"服务器是否拒绝了调用方"，
  应用层结果（key 不存在）是响应不是错误。
- **inclusive 起点**：`watch(key, start)` 以 `WatchOptions::with_start_revision(start)` 订阅，语义
  与接缝文档一致：流从该修订**含**自身开始。
- 流转发：`watch` 返回前把 gRPC 流交给一个后台转发任务，映射每个响应为接缝 outcome 写入
  `mpsc` 通道（容量 = adapter 的 `WATCH_CAPACITY`）；流结束映射为 `Connect`。

### 真实 wire 的三个事实（映射位置的依据）

1. **`Established` 永不产生**：`etcd-client` 的 `Client::watch` 自己发送创建请求并等待 created
   确认（其实现内部 `assert!(resp.created())`），确认不进入调用方的流。接缝的 `Established` 只有
   scripted client 会产生；真实 client 之下，"已建立"由 `watch` 调用成功返回表达。
2. **`Fragmented` 不出现在真实 wire**：`etcd-client` 不请求 fragment（`WatchOptions` 不设置该
   位），也不把 fragment 标记透传为独立结果——分片到达时就是普通的事件子集响应，adapter 逐个
   处理。接缝的 `Fragmented` 由 scripted adapter 测试覆盖。
3. **canceled 原样透传**：取消响应（含 `compact_revision`）进入调用方的流 →
   `WatchOutcome::Canceled`；adapter 以全量重载应答。

## mock server（`client/mock_server.rs`，cfg(test) + client feature 门控）

进程内 etcd v3 替身：**官方协议、内存存储、测试直接驱动的控制面**。它不是 etcd 的仿真，它生产
adapter 契约观察到的那些情形，别的什么都不做。

- **协议来源**：`proto/etcdserverpb/{rpc,kv,auth}.proto`（从 etcd-client 0.14.1 vendor，剥离
  gogo/annotations）经 tonic-build 生成 **server** 端（`build_client(false).build_server(true)`）。
  `etcd-client` crate 只发布 client 类型、无 server 半边，这是 vendor 的原因。生成代码仅在测试
  编译（`proto.rs` 整个 `#[cfg(test)]`），生产链接不出现。
- **服务面**：KV 服务实现 `range`（读最新修订；最新是 tombstone 即 key 不存在），其余 RPC
  `unimplemented`——adapter 只驱动这一个 KV RPC；Watch 服务实现 `watch` 流。
- **存储**：单 key 完整修订历史 `Vec<(i64, Vec<u8>)>`，空值即 delete tombstone。`range` 返回最新
  非空修订；watch 重放 `mod_revision >= start` 的历史（inclusive），每个事件一个响应。
- **控制面**（测试句柄直接调用）：
  - `put(bytes) -> i64` / `delete() -> i64`：追加修订并广播 live 变更；
  - `compact(rev)`：此后任何 `start <= rev` 的订阅在创建确认后收到取消响应（携带 `rev`）——与
    真实 etcd 的语义一致（被压掉的历史无法无缝重放）；
  - `set_require_auth(bool)`：所有操作返回 `permission_denied`（→ client 分类为 `Auth`）。
- **live tail**：`tokio::sync::broadcast` 订阅先于创建确认发出，重放期间落地的变更走 live 通道
  再次到达，adapter 的修订过滤吸收重复；订阅者落后（`Lagged`）→ 取消响应——真实 etcd 对追不上
  的 watcher 同样取消。
- **端口**：先 `TcpListener::bind("127.0.0.1:0")` 取得实际地址，再以
  `serve_with_incoming(TcpListenerStream)` 交给 tonic——同一个 listener 全程持有，无 free-port
  竞态。
- **时序确定性**：`etcd-client` 的 `watch` 调用在返回前等待 created 确认，而 mock 的确认在订阅
  live 通道之后发出——因此测试中 `watch(...)` 返回后再 `put`，变更必然被 live tail 送达，无竞态。
- **不做的事**：TLS、lease、txn、多 endpoint failover、集群行为。failover 属于接缝之下的
  `etcd-client` 自身配置，本任务的测试不覆盖；adapter 已把连接失败折叠为 `Unavailable`。

## 栈纪律（第二套网络栈的边界）

- `nico-config-etcd` 的 `client` feature（**默认开**，供测试与显式接入）链接 tonic 0.12.3 +
  hyper 1.12.0——这是继 Pingora 之后仓库里第二套传输栈，是已知且被文档化的取舍。
- 产品二进制不链接：`nico-server` 的 `etcd` feature 默认关；
  `cargo tree -p nico-server --no-default-features --features traffic` 与 `cargo tree -p nico-server`
  均无 tonic/etcd-client。堆栈检查按 spec 的方式每 release 复核。
- adapter 层（`source.rs`）与接缝（`client.rs`）零网络类型：不开 `client` feature 时 crate 仍然
  完整实现 `ConfigSource`（scripted mock 驱动其 14 条语义测试）。

## 测试布局

- **adapter 语义**（`tests/mod.rs`，scripted client，无网络）：14 条，`11-14` 交付，本任务未动。
- **seam 映射 + 全链**（`tests/e2e.rs`，真 client ↔ mock server，真实 gRPC wire）：12 条。seam 层
  断言协议形状 → 接缝类型的映射（重放、live、delete、取消、分类）；全链断言
  `EtcdConfigSource` over `GrpcEtcdClient` 的组合（load/watch 快照、compaction → ResyncRequired、
  Unavailable 折叠）。
- fragment / Established 语义不在 e2e 中（wire 事实见上）；它们的覆盖在 scripted adapter 测试。
