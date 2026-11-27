# 跨副本共享 token budget 与 tenant quota

## Goal

让同一部署的多个 nico 副本共享 convert 路由的 token budget，使 `(route_id, tenant)` 和
`(route_id, subject)` 的固定窗口在副本之间保持一致；保留现有进程内实现作为单副本模式，
并通过统一的配额领域接口切换 etcd 或 Redis 共享实现。

用户价值是：负载均衡把相同租户的请求分到不同副本时，预算仍代表部署级额度，而不是每个副本
各自放行一份额度。

## Confirmed facts and constraints

- [`BudgetStore`](crates/nico-http/src/quota.rs:59-70) 当前是同步的独立 `check` + `charge`；
  `InProcessBudgetStore` 使用固定窗口和有界 LRU。它的 crate-private 可见性、无异步 I/O 和
  `charge -> ()` 返回值不足以支撑外部实现。
- [`HttpStateHandle`](crates/nico-http/src/state.rs:153-191) 持有 `Arc<dyn BudgetStore>`，该状态
  跨配置 reload 存活，是共享实现的生命周期边界。请求在
  [`upstream_peer`](crates/nico-http/src/proxy/hooks/request.rs:126-157) 前检查，在
  [`record_tokens`](crates/nico-http/src/proxy/record.rs:296-329) 成功响应后按 provider reported
  `input + output` 记账。
- tenant quota 的 key 语义已确定：有 tenant 使用 `(route_id, tenant)`，无 tenant 使用
  `(route_id, subject)`；不跨 route、provider/member 或 request-count rate limit 共享。
- 现有 `nico-config-etcd` 只实现配置源 `get/watch`；历史设计排除了 transaction/lease，因此
  不能直接作为 quota API。仓库当前没有 Redis 客户端或 Redis adapter。
- 业务语义保持 post-hoc：provider token 在 dispatch 前未知，所以允许一次成功请求把窗口推过
  limit，后续 admission 才拒绝；本任务不承诺零超额硬上限。

## Requirements

### R1 统一、可扩展的配额领域接口

- 将配额存储契约提升为 `nico-http` 可注入的异步 domain seam，公开实现所需的 key、policy、
  check result、charge 请求和错误类型；请求路径不暴露 etcd/Redis 类型。
- `check` 和 `charge` 是两个业务时间点；每个 backend 操作内部必须原子：`check` 按 expiry
  读取/创建/重置窗口并判断是否超限，`charge` 去重并追加实际 token。不能把远端 read-modify-write
  放在 Rust 客户端；两个时间点之间保留既定 post-hoc overshoot 语义，不要求 admission 预留未知 token。
- `charge` 必须带一次成功 provider exchange 的稳定 `charge_id`，后端按 `(namespace, quota_key,
  charge_id)` 幂等；即使追加后超过 limit 也必须写入。
- 领域错误至少区分 `Exceeded` 与 `Unavailable`、`Timeout`、`Conflict`、`Unauthorized`、
  `Corrupt`、`Invalid`；基础设施错误不得伪装成 `429 token_budget_exceeded`。
- 固定窗口、route/effective identity key、policy 随请求传入、缓存不清账等现有语义保持不变；
  共享时间使用后端 wall-clock/expiry，不跨副本传递 `tokio::time::Instant`。

### R2 两个独立后端实现

- 提供 etcd adapter 和 Redis adapter；两者实现同一个 domain seam，不能在请求路径中出现后端分支。
- etcd adapter 使用 quota-specific transaction/lease/TTL client seam；不能改造或复用现有
  `ConfigSource` / `EtcdClient` 的 get/watch API。
- Redis adapter 使用服务端脚本或事务完成原子窗口更新、TTL 和 charge 去重；不能使用客户端读改写。
- 两个 adapter 统一 namespace、过期、幂等和错误映射；endpoint、凭证、远端 body 不进入客户端响应
  或请求日志。未来新增后端只需新增 adapter 和契约测试。
- `nico-http` 不依赖具体 KV 客户端；adapter 放在独立存储 crate 或 `nico-server` 的存储边界，
  通过 feature/装配控制依赖。

### R3 运行时与配置

- 不声明 quota storage 时继续使用 `InProcessBudgetStore`，现有默认部署行为不变。
- bootstrap 增加独立于 `config_source` 和 route `token_budget` 的可选 quota store 配置；共享模式
  显式选择 `etcd` 或 `redis`，并配置 namespace/prefix、endpoint、TLS/auth、操作超时和连接生命周期。
- 共享模式必须显式配置 `on_unavailable: fail-closed | fail-open`；本地模式不需要该字段，避免
  产生无效配置。
- store 与 `HttpStateHandle` 同生命周期，跨 reload 保留累计窗口；reload 只改变后续调用的 policy。
- adapter/装配层拥有连接建立、TLS、认证、重连、取消、优雅关闭和资源上限；`nico-http` 只看到
  domain error。

### R4 故障与记账策略

- `Exceeded` 始终返回现有 `429 token_budget_exceeded`。
- 共享存储无法确认 admission 时，按 bootstrap 的 `on_unavailable`：
  - `fail-closed` 返回独立稳定的 `503 quota_store_unavailable`；
  - `fail-open` 继续请求，同时产生 degraded 指标和结构化日志，并记录配额绕过风险。
- 事务冲突、超时、鉴权失败、数据损坏都必须映射到可观测的 domain error；不能清零损坏窗口来绕过
  预算。
- 成功响应后的 charge 无法重写已提交响应；必须使用幂等 charge、有限重试和明确的 at-least-once
  上界，charge 失败不能静默丢账或重复扣账。

### R5 可验证交付

- domain contract 测试覆盖 local/etcd/Redis：固定窗口、过期重置、post-hoc 越额、并发原子更新、
  duplicate charge、namespace 隔离和 typed errors。
- 多客户端集成测试证明两个独立 nico 实例共享同一 tenant key 和无 tenant subject key 的窗口；
  超限后 provider 不再被联系。
- 故障测试证明 fail-closed/fail-open 都按配置生效，存储错误不会变成 `429`；恢复后窗口状态和
  幂等记账符合定义。
- 生命周期测试证明 store 跨 reload 保留，候选配置失败保留 LKG，shutdown 能取消外部操作。
- traffic role、默认 local 路径、现有 tenant 映射、reload 语义、依赖边界、全量 gates 和 500 行
  文件预算均不回归。

## Out of scope

- 计费、价格表、网关 tokenizer、context-window 估算和 provider 新 kind。
- tenant hierarchy、跨 route shared pool、按 provider/kind 分桶和 key 管理 API。
- config-nacos、TLS 热加载、主动健康检查等独立能力。
- 将现有配置源 etcd adapter 改造成 quota store，或把客户端 read-modify-write 宣传为原子配额。
- 在没有幂等/故障边界的情况下用本地缓存或异步补账冒充精确集群预算。

## Key decisions

- 后端采用可扩展方式：etcd 和 Redis 都是首批 adapter，共享一个 domain interface；请求路径和
  `nico-http` 不知道具体后端。
- 故障策略由用户通过 bootstrap 配置选择 `fail-closed` 或 `fail-open`；网关不替用户决定可用性
  与配额绕过的取舍。两种模式都必须可观测，且错误契约不同。
- 共享版本保留 post-hoc、允许 in-flight overshoot 的现有语义，不引入未知 token 的 reservation，
  不宣传硬上限。

## Technical notes

设计已写入 [`design.md`](design.md)，覆盖 domain seam、async 调用链、HttpStateHandle 生命周期、
etcd/Redis adapter、bootstrap 配置、故障映射、幂等 charge、兼容和回滚。执行步骤与验证命令写入
[`implement.md`](implement.md)。
