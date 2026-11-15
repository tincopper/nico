# 设计：etcd 配置源（接口先行，边界 mock）

## 1. 不变量与失败模式

| 不变量 | 违反时的失败模式 |
|---|---|
| **I1 适配器不认识网络。** 一切 etcd 语义经 `EtcdClient` trait 进入；trait 的 mock 是测试里的"真实环境"。 | 适配器直接持有 `etcd-client`，测试要么需要真 etcd，要么开始 mock 第三方类型——接缝就没了。 |
| **I2 一个 key = 一份完整文档。** etcd 里不做 patch：put 事件就是新快照，delete 就是不可用。 | 按 event 增量拼接会把两份半文档当成一份新的，digest 无法稳定。 |
| **I3 删除与 auth 失败是 `Unavailable`，不是空配置。** | 一个空文档会通过解析并发布"清空一切"的快照，LKG 被合法地覆盖掉。 |
| **I4 compaction 与 fragment 都是 `ResyncRequired`。** | 拿压缩后的 revision 继续增量，中间的历史事件就永远丢了。 |
| **I5 revision 单调、不重不漏。** `watch(R+1)` 起，`mod_revision <= R` 丢弃，每个新 revision 恰好一次。 | 重复事件造成假 reload；漏事件造成旧配置继续服务而无人知道。 |
| **I6 消费者落后折叠为一次 resync。** | 无界缓冲内存膨胀，或有界通道静默丢事件——后者是 I5 的违反。 |

## 2. 接缝在哪

```
EtcdConfigSource (实现 ConfigSource)
        │ 调用
        ▼
EtcdClient trait          ← 「真实环境」的边界
  get(key)                 │
  watch(key, start) ──► WatchOutcome 流（类型化的 etcd watch 协议）
        │                        ▲
        │                        │ script 化实现（mock）= 测试里的真实 etcd
        ▼                        │
适配器把结果映射成 ConfigEvent ──┘
```

`EtcdClient` 是 `pub(crate)`：它只被适配器消费，未来真实实现也在本 crate 内。外部世界只看到
`ConfigSource`。

## 3. 类型化的 watch 协议

etcd 的 watch 流有五种结果，各有不同的处置，所以是五个变体而不是一个带 Option 的结构：

| 变体 | etcd 含义 | 适配器处置 |
|---|---|---|
| `Established` | watch 创建成功 | 继续读 |
| `Events(Vec<WatchEvent>)` | 一批事件，共享头 revision | 逐个映射：put → `Snapshot`，delete → `Unavailable` |
| `Canceled { compact_revision }` | 订阅被取消（历史已压缩） | `ResyncRequired` 并结束 |
| `Fragmented` | 服务端把事件分片送达 | `ResyncRequired`：拒绝增量，整份重载 |
| （流结束/错误） | 连接断、auth 拒绝 | `Unavailable` 错误事件 |

`WatchEvent::Put { value, mod_revision }` / `WatchEvent::Delete`。`mod_revision` 映射
`Revision::comparable`。

## 4. load 与 watch 的衔接

```
load() ──► client.get(key)
   │           ├── Some { value, mod_revision } ──► ConfigVersion(rev+bytes)（先过 max_bytes / UTF-8）
   │           ├── None ──► Unavailable（key 不存在不是空文档，I3）
   │           └── Err ──► Unavailable
   ▼
watch(from = Some(rev)) ──► client.watch(key, rev + 1)   // inclusive（I5）
   │
   ▼ 后台任务：WatchOutcome → ConfigEvent
   ├── Established → 继续
   ├── Events → 逐个：rev <= last 丢弃（I5）；put 过 max_bytes/UTF-8 后发 Snapshot；
   │            delete → Unavailable 错误事件
   ├── Canceled / Fragmented → ResyncRequired，结束
   └── Err → Unavailable 错误事件，结束
```

`last` 从 `from` 初始化并在每次接受事件时更新，所以重连重订阅也不会重放已见 revision。

## 5. 错误映射

| EtcdError 形态 | ConfigSourceError |
|---|---|
| 连接失败、auth 拒绝、超时、key 不存在（load） | `Unavailable { source_name: "etcd", source }` |
| 文档 > max_bytes（load 与事件同一条路径） | `TooLarge { size, limit }` |
| 值不是 UTF-8 | `NotUtf8` |

`EtcdError` 是本 crate 的类型化错误（连接/auth/限流/编码四类），mock 可以精确驱动每一条；
真实客户端未来把自己的错误映射到它。

## 6. 消费者落后

有界 `mpsc`（容量 = 配置的 watch 容量，沿用契约的默认）满时，剩余事件折叠成一次
`ResyncRequired` 后结束——与 `fake.rs` 的规则一字不差，由同一个后台任务模式承载。适配器不
缓存历史，pipeline 整份重载（I6）。

## 7. failover 的归属

endpoint 列表、连接池、重连循环都属于**真实 `EtcdClient` 实现**的职责：etcd 的 client SDK 已经
把这些做对，适配器再实现一遍就是第二份要维护的逻辑。适配器的承诺是：client 报告的一切失败都
折叠成 `Unavailable`，pipeline 的重载循环负责"稍后再试"。这是 roadmap §5 第 6 条的答案——
不是"适配器做 failover"，而是"failover 在接缝之下，适配器对它无感"。

## 8. 为什么不引入 `etcd-client`

roadmap 自己写过：etcd 客户端是 tonic/gRPC 栈，与 Pingora 并存就是第二套传输栈（§5 第 6 条的
检查项）。在没有任何可连环境时引入它，只会让 `Cargo.lock` 先背上依赖，而语义全部无法测试。
`EtcdClient` trait 把这个决定推迟到"有环境可连"的那一天，且那天只需要写一个实现类。

## 9. 备选方案与取舍

| 备选 | 为什么不做 |
|---|---|
| **直接引入 `etcd-client` 并 mock 它的类型** | mock 第三方类型脆：每次升级都要追着改。自己的 trait 是稳定接缝。 |
| **把 watch 事件拼成 patch** | I2：契约明确无 patch，每个事件都是完整文档。 |
| **delete 之后继续 watch** | I3：key 没了之后的"继续"没有意义，pipeline 需要知道配置源已经不可用。 |
| **在适配器里做 endpoint failover** | §7：属于真实 client 的职责，适配器对连接无感。 |
| **无限通道避免 resync** | I6：无界缓冲是内存膨胀；折叠 resync 是契约写明的正确行为。 |
| **`mod_revision` 用 `equality_only`** | watch 的增量过滤需要序号比较（I5），comparable 是语义正确的选择。 |

## 10. 回滚

新 crate + 一个默认关闭的 feature，产品二进制不链接它。回滚 = 删目录与 feature 行。

## 11. 测试策略

| 层 | 内容 |
|---|---|
| load | 命中 / key 不存在 → Unavailable / 超 max_bytes → TooLarge / 非 UTF-8 |
| watch | inclusive 起点不重不漏；compaction → ResyncRequired；fragment → ResyncRequired；delete → Unavailable；auth 错误 → Unavailable；事件超 max_bytes → TooLarge；旧 revision 丢弃；消费者落后折叠 resync |
| 协议 | Established 后才有事件；Canceled 后流结束 |
| 回归 | 全树测试、clippy、fmt、`cargo tree -d`、traffic 角色编译 |
