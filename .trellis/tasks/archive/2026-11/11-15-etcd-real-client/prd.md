# 真实 etcd client：gRPC 接线 + 进程内 mock server

## Goal

实现 `EtcdClient` 接缝的 gRPC 实现（`etcd-client` crate），并用进程内 etcd v3 mock server（tonic
实现官方 rpc.proto，内存 KV + watch 历史/compaction/auth）端到端验证；不需要真实 etcd 环境。

## Requirements

- **真实 client**：`EtcdClient` 接缝的 gRPC 实现（`etcd-client` crate，feature `client`），get/watch
  映射到接缝的类型化协议，gRPC 状态分类为 `Connect` / `Auth`。
- **mock server**：进程内 etcd v3 gRPC 服务（从 vendored rpc.proto/kv.proto/auth.proto 经
  tonic-build 生成 **server** 端——`etcd-client` crate 自身只发布 client 类型），内存 KV + 完整修订
  历史 + 控制面（compact / require_auth）。测试用它替代真实 etcd 环境。
- **端到端验证**：真实 client ↔ mock server 走真实 gRPC wire，验证接缝映射与协议语义在两端的
  组合，而不是只在 scripted mock 上验证 adapter。
- **栈纪律**：tonic/hyper 是已知的第二套栈；`nico-server` 的 `etcd` feature 默认关闭，产品二进制
  不链接它；`cargo tree -d` 的发现写入文档。

## 已确认的 wire 事实（决定验收口径）

真实 wire 上有三个事实，来自 `etcd-client` 0.14.1 的实现与 vendored proto：

1. **`Established` 永不产生**：`etcd-client` 的 `Client::watch` 自己发送创建请求并等待 created
   确认（内部 `assert!(resp.created())`），确认不进入调用方的流。接缝的 `Established` 变体只有
   scripted client 会产生。
2. **`Fragmented` 不出现在真实 wire**：`etcd-client` 不请求 fragment（`WatchOptions` 不设置），也
   不把 fragment 标记透传为独立结果——分片到达时就是普通的事件子集响应，adapter 逐个处理。接缝
   的 `Fragmented` 变体由 scripted adapter 测试覆盖（`a_fragmented_watch_is_a_resync`）。
3. **canceled 原样透传**：取消响应（含 `compact_revision`）进入调用方的流，映射为
   `WatchOutcome::Canceled`。

因此：mock server 不模拟 fragment；fragment 语义的验证保留在 adapter 层。

## Acceptance Criteria

- [x] `GrpcEtcdClient`（feature `client`，默认开）：get → `KvEntry`；watch 从 start_revision
      （inclusive）流式映射 Events / Canceled；gRPC 状态 → `EtcdError::{Connect, Auth}` 分类
      （Unauthenticated | PermissionDenied → Auth，其余 → Connect）。
- [x] mock etcd server（cfg(test) 门控）：官方 rpc.proto 生成的 KV + Watch 双服务，内存存储 +
      历史 + 控制面（compact / require_auth）；协议形状与真实 etcd 一致（created 确认、tombstone
      delete 事件、取消响应携带 compact_revision）。
- [x] 端到端测试 12 条（真 client ↔ mock server，真实 gRPC wire）：seam 层 8 条——load 命中最新
      修订、缺失 key 为 None、auth 拒绝 → `Auth`、inclusive 起点重放、live 变更、delete →
      `Delete` 事件、compaction 起点 → `Canceled{compact_revision}`、越过后重新订阅正常；全链 4
      条——adapter over 真实 client 的 load+watch 快照、compaction 起点 → `ResyncRequired`、缺失
      key / auth 拒绝 → Unavailable。
- [x] 特性门控：`cargo tree -p nico-server --no-default-features --features traffic` 与默认构建
      均不出现 tonic/etcd-client；`client` feature 引入 tonic 0.12.3 + hyper 1.12.0（第二套栈），
      记录在 `design.md`。
- [x] 全树回归（`--all-features` 含本任务测试）、clippy、fmt、README 限制段落更新、归档。提交由
      用户单独指示。

## Notes

- Keep `prd.md` focused on requirements, constraints, and acceptance criteria.
- Lightweight tasks can remain PRD-only.
- For complex tasks, add `design.md` for technical design and `implement.md` for execution planning before `task.py start`.
