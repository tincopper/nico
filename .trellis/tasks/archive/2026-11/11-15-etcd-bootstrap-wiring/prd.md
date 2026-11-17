# bootstrap `type: etcd` 接线

## Goal

让产品进程能以 `config_source.type: etcd` 启动：bootstrap 按 feature 接受 etcd 源声明，构造
`GrpcEtcdClient` + `EtcdConfigSource` 交给既有管线；整条接线在进程内 mock server 上端到端验证，
不需要真实 etcd 环境。顺带闭合 roadmap §5 最后一个取证项（endpoint failover）。

## 现状

- 管线（`server.rs` / `reload.rs`）已经完全 generic over `Arc<dyn ConfigSource>`——本任务不碰它。
- `bootstrap.rs` 的 `RawSource` 只认识 `local`（`path` + `watch` + `max_bytes`），其余类型在
  `load` 即拒绝（`UnsupportedSource`）。
- `nico-server` 已有 `etcd = ["dep:nico-config-etcd"]` feature（默认关），依赖存在但无人消费。
- `nico-config-etcd` 的 mock server 是 `cfg(test)` 私有模块，跨 crate 测试用不了。

## Requirements

- **schema 按类型分形**：`config_source` 的体是 per-type 的——`local` 保持 `{ path, max_bytes,
  watch }`；`etcd` 是 `{ endpoints, key, max_bytes }`。`endpoints` 非空 URL 列表（多 endpoint
  原样传给 client，failover 在接缝之下）；`watch`/`path` 对 etcd 不是字段（写进去就是
  unknown-field 解析错误）。
- **编译期门控的接受面**：`type: etcd` 只有在 `nico-server` 编译进 `etcd` feature 时才被接受；
  没有该 feature 的构建在 `load` 即拒绝，错误说明这个构建服务哪些源类型。验证过的
  `BootstrapConfig` 永远不表示"这个构建无法服务的源"（既有原则，见 bootstrap.rs 模块文档）。
- **构造**：local 路径不变（同步）；etcd 路径在 main 里用一个临时的 current-thread tokio
  runtime 完成 `GrpcEtcdClient::connect`（connect 是 async，而 Pingora 的 runtime 此后才开始），
  包成 `EtcdConfigSource`。构造逻辑集中在一个可测的函数里，`lib.rs` 的 main 不含分支逻辑。
- **mock server 跨 crate 复用**：`nico-config-etcd` 增加 `mock-server` feature（默认关，要求
  `client`），把生成协议与 mock server 的 `cfg(test)` 门控放宽为 `cfg(any(test, feature =
  "mock-server"))` 并公开 API；`nico-server` 的 dev-dependencies 用它，使接线测试跑真实 gRPC wire。
- **failover 取证**：`GrpcEtcdClient` 的 seam e2e 增加一条：连接含一个不可达 endpoint 的列表，
  操作仍成功/或如实记录 connect 的严格性。roadmap §5 的最后一项据结果闭合。

## Acceptance Criteria

- [x] `type: etcd` + `etcd` feature：bootstrap 接受并携带 endpoints/key；缺 endpoints、空
      endpoints、非法 URL、空 key 在 `load` 拒绝。
- [x] `type: etcd` 无 `etcd` feature：`UnsupportedSource`，错误文本列出该构建支持的类型。
- [x] `local` schema 行为不变（既有测试全绿）；etcd 变体里写 `path`/`watch` 是解析错误。
- [x] 构造函数：etcd bootstrap → `EtcdConfigSource` over `GrpcEtcdClient`（对 mock server 的
      load 成功）。connect 惰性（探针实证）：endpoint 全部不可达也构造成功，失败以
      `Unavailable` 进入管线重载循环——启动不阻塞于 etcd 可达性。
- [x] nico-server 集成测试：bootstrap 文件 `type: etcd` → mock server → 整条管线 load 出文档
      （真实 gRPC wire）。
- [x] failover 取证落地（roadmap §5 最后一项），结果如实记录。
- [ ] 全树回归、clippy、fmt；`cargo tree` 确认默认构建与 `traffic` 角色仍不链接 tonic。
- [x] README 限制段与任务文档更新。提交由用户单独指示。
