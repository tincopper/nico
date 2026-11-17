# 设计：bootstrap `type: etcd` 接线

## 总览

管线的每一层都已就位：`EtcdConfigSource` 实现 `ConfigSource`（11-14），`GrpcEtcdClient` 在进程内
mock server 上取证（11-15），`server.rs` / `reload.rs` 完全 generic over `Arc<dyn ConfigSource>`。
缺的只有一段：进程启动时，一个 `config_source.type: etcd` 的 bootstrap 文件如何变成管线手里的
`Arc<dyn ConfigSource>`。本设计补这一段，不动管线。

三条不变量沿用 bootstrap 模块文档的既有原则：

1. **验证过的类型不表示无法服务的源**——`type: etcd` 只有编译进 `etcd` feature 的构建才接受；
2. **bootstrap 文件决定"从哪里读"，配置源内容永远改不了它**；
3. **schema 严格**：每个类型有自己的字段集，写错字段是启动错误。

## per-type schema（两段式解析）

`config_source` 的体按类型分形：`local` 是 `{ path, max_bytes, watch }`；`etcd` 是 `{ endpoints,
key, max_bytes }`。`watch`（fs notify/signal）是本地文件源独有的触发方式——etcd 源自带推送式
watch，没有这个字段；把 `path` 写进 etcd 源声明是 unknown-field 解析错误，反之亦然。

serde 的 `deny_unknown_fields` 与 tagged enum 不能同时工作，所以解析分两段：

1. 外壳 `RawBootstrap` 把 `config_source` 读作 `serde_norway::Value`，其余字段照旧；
2. 按 `config_source["type"]` 分派：`"local"` → `RawLocalSource`，`"etcd"`（仅 `etcd` feature 下
   编译）→ `RawEtcdSource`——两个结构各自 `deny_unknown_fields`，缺字段、错字段、错类型都在这一步
   变成 `Parse` 或 `UnsupportedSource` 错误；未知类型报 `UnsupportedSource { found }`，错误文本
   列出该构建实际支持的类型。

验证规则（etcd 变体）：`endpoints` 非空、每条是合法形状（`http://host:port` 或裸 `host:port`；
本构建的 etcd client 没编 TLS feature，`https` 在 `load` 即拒绝）、`key` 非空；
`max_bytes` 共享既有规则。多 endpoint 原样交给 client——failover 是 client 自己的行为（见下）。

## `BootstrapConfig` 的形状

持有一个私有枚举，携带纯数据（不引用任何依赖类型）：

```rust
enum SourceSelection {
    Local { path: PathBuf },
    #[cfg(feature = "etcd")]
    Etcd { endpoints: Vec<String>, key: String },
}
```

`WatchMode` 从顶层字段移进 `Local` 变体（etcd 没有它）；`max_bytes` 保持共享字段——它是文档大小
预算，两种源都要。`local_config()` 被 `source()` 取代，调用方 match；既有访问者
（`source_max_bytes` 等）不动。没有 `etcd` feature 的构建里枚举只有 `Local`，etcd 分支连编译都不
存在。

## 构造：client 的 runtime 必须活着（探针实证）

`GrpcEtcdClient::connect` 是 async，而 `main` 在 Pingora 接管之前没有 runtime。两个探针结论决定
了构造设计：

1. **failover 成立**：`connect(&[dead, live])` 成功，后续操作正常——一个不可达 endpoint 不破坏
   client，故障转移在 `etcd-client` 之下完成（roadmap §5 最后一项的证据）；
2. **runtime 生命周期 = client 生命周期**：connect 的 runtime 被丢弃后，client 的 transport 任务
   随之死亡，之后的每个操作都报 `transport error`；但只要该 runtime 活着，`get`/`watch` 可以从
   **任何** runtime 驱动（`GrpcEtcdClient` 是 `Send + Sync`，调用方的 executor 只轮询调用自身的
   future）。

因此构造分两半：

- **etcd runtime 守护线程**：一个常驻 OS 线程跑 current-thread tokio runtime，
  `block_on(connect)` 之后 `block_on(pending())` 驱动到进程结束。client 的 transport 任务钉在这里，
  与 Pingora 的 runtime 互不干扰；
- **`build_source(&BootstrapConfig)`**：唯一的源构造点。`Local` → `LocalFileSource`（同步，照旧）；
  `Etcd`（cfg）→ 守护线程 connect → `EtcdConfigSource::new(key, max_bytes, client)`。`main` 不含
  分支逻辑，这个函数可测。

**启动语义（探针实证）**：`etcd-client` 的 connect 是惰性的——即使列表里所有 endpoint 都不可达，
connect 也成功；不可达在第一个操作时以 `Connect` 错误浮现。因此启动**不阻塞**于 etcd 的可达性，
连接失败以 `Unavailable` 进入管线既有的重载循环（与"运行中断连"同一条路径）。守护线程仍是必需
的：connect 成功即意味着 transport 任务已经产生，它们必须有一个活着的 runtime。守护线程是
detached 的：进程退出不需要 join，取消协议仍由既有的单一 `CancellationToken` 表达。

## mock server 的跨 crate 复用（`mock-server` feature）

接线测试要跑真实 gRPC wire，mock server 必须能被 `nico-server` 的测试使用。`nico-config-etcd`
新增 feature `mock-server`（默认关，要求 `client`）：

- `prost`、`tokio-stream` 从 dev-dependencies 复制为 optional 依赖（生成代码在非测试编译下需要
  它们）；tonic-build 的 proto 产物与 `mock_server.rs` 的门控从 `#[cfg(test)]` 放宽为
  `#[cfg(any(test, feature = "mock-server"))]`，类型与构造函数 `pub`；
- 生产任何目标都不启用它：它只出现在 `nico-server` 的 **dev-dependencies** 里。feature 存在的
  唯一理由是"让别的 crate 的测试跑协议替身"。

## 测试布局

- **bootstrap schema**（`bootstrap/tests.rs`，两种 feature 组合）：etcd 接受/拒绝矩阵（缺
  endpoints、空 endpoints、非法 URL、空 key、写 `path`/`watch` 进 etcd、无 feature 拒绝）；local
  既有行为不变。
- **构造**（`build_source` 测试，`etcd` + `mock-server` feature）：bootstrap 文件 → mock server →
  `source.load()` 出文档——`Arc<dyn ConfigSource>` 整链在真实 wire 上组合。
- **failover 取证**（`nico-config-etcd` e2e）：dead endpoint 在列时 connect 成功、操作正常。
- 栈纪律复核：`cargo tree -p nico-server`（默认、`traffic`）无 tonic；`etcd` feature 才引入。

## 不做的事

- 不给管线加源类型分支：`server.rs` / `reload.rs` 对源一无所知，保持如此。
- 不实现 etcd 的 TLS/认证凭据配置：`etcd-client` 支持（`ConnectOptions`），但第一版接线不暴露；
  需要时是 bootstrap schema 的纯增量。
- 不在 bootstrap 里支持 `type: nacos` 或其他源：出现第二个真实消费者再谈。
