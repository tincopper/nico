# 配置源实现研究：etcd 优先

日期：2026-11-03  
范围：Rust 网关的 `ConfigSource` 实现；第一期只落地 local + etcd，Nacos 推迟。  
说明：本文只记录已核对的一手资料与仍需按锁定版本验证的项，不包含产品代码。

## 0. 结论

1. **第一期 etcd 方案可行。** 使用 etcd v3 gRPC KV + Watch，单 key 保存完整文档；启动 `Range` 获取当前 revision 和 value，随后从 `revision + 1` 开始 watch。
2. **watch 的 `start_revision` 是包含式（inclusive）**。若启动 Range 返回 revision `R`，下一次 watch 应从 `R + 1` 开始，避免重复应用同一版本；重复时也可由 digest 去重。
3. **compaction 与“跟不上进度”都会导致 watch 被取消**，响应带 `compact_revision`。此时不能从旧 revision 重试，必须重新 `Range` 全量读取，再建立新 watch。
4. **etcd 单 value 受服务端 `max-request-bytes` 限制**。所核对 release-3.5 源码默认 `1.5 * 1024 * 1024` 字节（约 1.5 MiB），可通过服务端参数调整。网关 `max_bytes` 必须小于该限制；超过时应重新评估多 key 方案，而不是盲目调大 etcd 限制。
5. **Rust 侧候选客户端 `etcd-client` 0.21.1**（etcdv3/etcd-client）提供 etcd v3 API、Watch、Auth，基于 tonic/prost。其 Watch API 是低层双向流，需要调用方自行处理 `created/canceled/compact_revision/events`，这与本方案“adapter 只暴露完整快照事件”的边界一致。
6. **Nacos 不在本期实现。** 其 dataId/group/namespace、MD5、长轮询/gRPC 与多 key 一致性需要单独核查，不能把 etcd 的 revision 语义直接套用。

## 1. 证据快照

- etcd 源码分支 `release-3.5`，核对提交 `6264e226edc4a83c6546dcdc820e3b4cc7dd96c4`。
  - `server/embed/config.go`：`DefaultMaxRequestBytes = 1.5 * 1024 * 1024`。
  - `Documentation/dev-guide/apispec/swagger/rpc.swagger.json`：`WatchCreateRequest`、`WatchResponse` 字段。
- `etcd-client` 仓库 `etcdv3/etcd-client`，`master` 核对提交 `829fddd8c416630a0c962670afda5a559bf4b513`，`Cargo.toml` 版本 `0.21.1`。
- 以上是研究时快照，不是项目锁定版本。实施前必须把依赖固定到具体版本并重新核对 API/默认值；etcd 服务端版本也需固定。

## 2. etcd v3 Watch 语义

### 2.1 关键字段

`WatchCreateRequest`（来自 etcd swagger）：

- `key`、`range_end`：监听单 key 或范围。
- `start_revision`：从该 revision 开始（**包含**）；不设置表示“从现在开始”。
- `progress_notify`：无事件时周期性发送 progress response，便于确认 watch 仍存活。
- `prev_kv`：返回变更前的 KV。
- `filters`：服务端过滤事件。
- `fragment`：允许把大 revision 的响应拆成多个 watch response。

`WatchResponse`：

- `created`：创建 watch 的响应。
- `canceled`：watch 被取消。
- `compact_revision`：请求的 revision 已被压缩，或 watcher 跟不上进度时设置。
- `cancel_reason`：取消原因。
- `header.revision`：响应头中的当前 revision。
- `events`：PUT/DELETE 事件。
- `fragment`：该响应是否是大响应的一个片段。

### 2.2 正确的启动与续订顺序

```text
1. Range(key) -> value + header.revision = R
2. 编译并发布该快照
3. Watch(key, start_revision = R + 1)
4. 收到 created 响应
5. 处理 events；对每个 PUT 读取完整 value 或使用事件中的 value
6. 断线 -> 退避 -> 回到步骤 1
```

注意：

- `start_revision = R` 会重复收到 revision R 的事件；本方案选择 `R + 1`，同时用 digest 防护重复。
- 不能假设 watch 事件一定连续或永不丢失。断线、compaction、客户端落后都可能要求全量重读。
- DELETE 事件表示 key 不存在：运行期应保留 LKG 并标记 degraded，不能编译成空配置。

### 2.3 compaction 与 gap

按 etcd swagger 与 `etcd-client` 的注释：

- 当 `start_revision` 已被压缩，或 watcher 无法跟上 etcd 进度时，服务端会取消 watch，并设置 `compact_revision`。
- 客户端应把该 watcher 视为已取消，**不要用同一个 `start_revision` 重建**。
- 正确动作：丢弃当前 watcher，重新 `Range` 获取最新完整文档和 revision，重新编译并发布，再从新 revision + 1 建立 watch。

### 2.4 大响应与 fragment

- `WatchResponse.fragment = true` 表示大 revision 被拆分到多个响应；实现必须按 `watch_id` 聚合，直到 `fragment = false`，否则可能把半个 value 当成完整配置。
- 第一期单 key + 大小上限通常可避免触发，但仍应在 adapter 中处理或显式拒绝 fragment，不能静默忽略。
- `etcd-client` 是否自动聚合 fragment 需按锁定版本核对；当前设计不依赖客户端自动处理。

### 2.5 认证、TLS 与多 endpoint

- etcd v3 使用 gRPC；认证通常为用户名/密码，TLS 由客户端配置 CA/客户端证书/SNI。
- `etcd-client` 提供 `Client::connect(endpoints, options)`，支持多个 endpoint；实际负载均衡/failover 行为需按锁定版本验证。
- `Cargo.toml` 的 TLS features 包括 `tls`（rustls + ring）、`tls-aws-lc`、`tls-native-roots`、`tls-webpki-roots`、`tls-openssl` 等；选择会影响二进制依赖和系统库。
- bootstrap 只保存凭证引用，不能保存明文密码或私钥。

## 3. Rust 客户端候选：etcd-client

### 3.1 事实

- 版本：`0.21.1`；MSRV `1.80.0`；edition 2021。
- 依赖：`tonic 0.14`、`prost 0.14`、`tokio 1`、`http 1.1`、`tower`。
- 能力：KV、Watch、Lease、Auth、Maintenance、Cluster、Lock、Election、Namespace。
- Watch API：`Client::watch(key, Some(WatchOptions))` 返回 `WatchStream`；`WatchStream::message()` 读取响应；`cancel()` 取消；`request_progress()` 请求进度。
- `WatchOptions::with_start_revision(revision)` 设置包含式起始 revision。
- 该 crate 明确说明 Watch 是低层 API stub，调用方负责处理 created/canceled/compact/events。

### 3.2 与本方案的映射

```text
ConfigSource::load
  -> Client::get(key)
  -> 检查 kvs 非空
  -> 读取 value + header.revision
  -> 大小/digest 校验
  -> ConfigVersion { revision, digest, bytes }

ConfigSource::watch(from)
  -> Client::watch(key, WatchOptions::new()
         .with_start_revision(from + 1)
         .with_progress_notify(...))
  -> 读取 created 响应
  -> 消费 events；PUT 读取完整 value
  -> canceled/compact_revision -> ResyncRequired
  -> DELETE -> 配置不可用（不是空快照）
```

adapter 不应把 `WatchStream` 暴露给 `gateway-server`，只暴露 `ConfigEvent`，避免 etcd 类型泄漏到 reload 管线。

### 3.3 依赖风险

- `etcd-client` 使用 tonic/prost；与 Pingora/Hyper 的依赖可能存在重复版本。实施时必须运行 `cargo tree -d`，确认不会引入两套不兼容的 HTTP/2/TLS 栈。
- TLS feature 选择会影响编译时间、二进制大小和系统依赖；优先与项目现有 rustls 方案统一，避免同时引入 openssl。
- `0.21.x` 的 Watch API 与更早版本不兼容（README 提到 0.18 起变化）；必须锁定版本，不按旧示例编码。
- 具体 API 名称、fragment 行为、endpoint failover 和错误类型需在锁定版本上编译验证。

## 4. 单 key 与大小限制

- 所核对 etcd release-3.5 源码默认 `DefaultMaxRequestBytes = 1.5 * 1024 * 1024`。
- 该限制是服务端请求大小约束，实际可用大小还受 gRPC、TLS、客户端和服务端配置影响。
- 第一期建议：
  - `bootstrap.max_bytes` 设为不超过 1 MiB，留出协议和编码余量。
  - 对网关配置做体积预算：路由数量、上游数量、策略文本、注释和锚点都会占用 value。
  - 超过预算时先压缩/精简文档；仍不够再设计多 key，但多 key 必须全量重读并保证完整性，不能发布半个配置。
- 不建议为了让单个大文档通过而随意调大 etcd `max-request-bytes`；这会扩大故障和滥用面。

## 5. 与 ConfigSource 管线的对应

| 阶段 | etcd adapter 行为 | server 行为 |
|---|---|---|
| 启动 | `Range` 读取 key，校验存在和大小 | 解析/编译；失败则不 ready |
| 热更新 | watch PUT 后读取完整 value | 去抖、revision/digest 去重、编译、ArcSwap 发布 |
| 断线 | 返回错误，丢弃 watcher | 退避 + jitter，重新 `load` |
| compaction/gap | 发 `ResyncRequired` | 全量 `load` 并重新编译 |
| key 删除 | 发 `KeyUnavailable`/`ResyncRequired` | 保留 LKG，标记 degraded，不发布空配置 |
| 认证失败 | 返回配置源错误 | LKG 服务 + 告警；启动阶段不 ready |
| 配置中心不可用 | watch/load 失败 | 继续使用 LKG，不阻塞请求 |

## 6. 测试清单

### 6.1 Fake / 单元测试

- 首次 load 返回 revision R，watch 从 R+1 开始。
- 重复 digest 不触发编译。
- revision 回退、乱序事件被丢弃。
- compaction 事件触发 `ResyncRequired`，随后全量 load。
- DELETE 事件不生成空配置。
- watch 断线、退避、重新 load。
- 慢消费者触发有界 channel 的 `ResyncRequired`，不无限缓存。
- 超大 value、非法 UTF-8、非法 YAML、未知字段、引用缺失均不发布。
- 编译失败保留 LKG；下一次合法配置成功发布。

### 6.2 真实 etcd 集成测试

- 固定版本 etcd 容器，开启认证和 TLS。
- 启动时 key 不存在 -> not ready。
- `put` 新配置 -> watch 触发 -> 新快照生效。
- 高频 put/delete -> 去抖与 single-flight 生效，无 reload 风暴。
- 停止 etcd -> 请求继续使用 LKG，指标 degraded。
- 恢复 etcd、watch 重连 -> 新配置恢复发布。
- 触发 compaction（写入足够多 revision 并 compact）-> watch 被取消，全量重载成功。
- 多 endpoint 场景下停止一个 endpoint -> 客户端 failover，配置仍可读。
- 超过 `max_bytes` 的 value -> 明确拒绝，不进入编译。
- 在途 SSE/H2 请求在 reload 期间继续使用旧 generation。

## 7. 仍需按锁定版本验证

- 目标 etcd 服务端版本及其 `max-request-bytes` 默认值/配置。
- `etcd-client` 锁定版本的 Watch API、fragment 聚合、错误类型、endpoint failover 行为。
- `etcd-client` 与 Pingora 的 tonic/hyper/tokio 依赖是否冲突，TLS feature 是否统一。
- 认证方式（用户/密码、证书、RBAC）与 token 过期/刷新行为。
- 是否使用 `progress_notify` 以及其频率；不能把“无事件”误判为断线。
- 是否需要 `prev_kv`；本方案只需要最新完整 value，通常不需要。
- 单 key value 的实际体积预算与是否触发多 key 设计。

## 8. Nacos（后续）

本期不实现，但保留 `ConfigSource` 扩展位。后续接入时必须单独核查：

- dataId/group/namespace 与网关配置的映射。
- 长轮询与 2.x gRPC 的版本差异。
- MD5 只能判等、不能排序，server 必须使用本地 generation。
- 多 key 发布可能导致半个配置，必须全量重读或使用一致性发布机制。
- 认证 token 刷新、推送乱序/重复、客户端断线重连。
- Rust 客户端生态与维护状态；不能把 etcd 的 revision 语义直接套用。

## 9. 参考链接

- etcd release-3.5 源码：<https://github.com/etcd-io/etcd/tree/release-3.5>
  - 默认请求大小：<https://github.com/etcd-io/etcd/blob/release-3.5/server/embed/config.go>
  - Watch RPC 字段：<https://github.com/etcd-io/etcd/blob/release-3.5/Documentation/dev-guide/apispec/swagger/rpc.swagger.json>
- etcd API 概念（官方文档入口）：<https://etcd.io/docs/>
- etcd-client：<https://github.com/etcdv3/etcd-client>
  - Watch 示例：<https://github.com/etcdv3/etcd-client/blob/master/examples/watch.rs>
  - Watch API：<https://github.com/etcdv3/etcd-client/blob/master/src/rpc/watch.rs>
  - Cargo 配置：<https://github.com/etcdv3/etcd-client/blob/master/Cargo.toml>
