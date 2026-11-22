# 设计:traffic 路由的 upstream 池(灰度分流)

## 1. 不变量与失败模式

1. **路由选择不依赖 body。** 池的关键是权重与调用方地址,与既有阶段约束一致;header/按模型分流
   是另一项能力,不在本任务。
2. **两段选择,各自用既有词汇。** 路由池选 upstream(本任务新增),upstream 成员池选机器(既有)。
   两层共享 nico-lb 的选择契约,不发明第二种选择器。
3. **候选列表是唯一的健康裁决点(既有规则的延伸)。** 路由池的候选 = 有至少一个可用成员的
   upstream;一个已知全灭的 upstream 不进候选,也就不会被策略恰好选中。全部候选为空 → 既有
   `502 upstream_unreachable`。
4. **池状态按代发布。** 路由池的选择器状态(round_robin 位置)与成员表一样按代构建、按身份继承,
   请求从它被路由的代中读取;reload 不把轮转归零。
5. **无池路由零行为变化。** 没写 `upstreams` 的路由走既有单目标路径;所有既有断言不回归。

失败模式:

| 失败 | 行为 |
|---|---|
| `upstreams` 与 `upstream` 同时写 | 发布失败:一个目标说两遍 |
| `upstreams` 写在 `provider` 路由上 | 发布失败:池是 upstream 域的词汇 |
| 空池 / 重复成员 / `weight: 0` / 引用未声明的 upstream | 发布失败,点名字段(traffic 域无 fallback,0 权重被拒) |
| `strategy` 无池 | 发布失败(既有规则,现在对两种池成立) |
| 某 upstream 全员被剔 | 路由池把它排除出候选,流量落其余 upstream |
| 全部 upstream 不可用 | `502 upstream_unreachable`(既有行) |

## 2. 契约

Schema 只动 traffic 路由的 `upstreams` 与共享的 `PoolStrategy`:

```yaml
routes:
  - id: api-canary
    host: api.example.com
    path_prefix: /v1
    upstreams:                    # 与 `upstream` 互斥;替代单目标
      - upstream: api-v1
        weight: 9
      - upstream: api-v2
        weight: 1
    strategy: ip_hash             # 可选;默认 weighted_random;仅允许与池同现
```

- 成员字段 `upstream` + `weight`(`1..=1000`,0 被拒——traffic 域没有 fallback,与 endpoint 的
  规则同源)。
- `PoolStrategy` 新增 `IpHash`:调用方完整地址会合哈希,粘性灰度;无地址的调用方(测试构造的
  会话)按轮转铺开——nico-lb 的 IpHash 自带该回退。llm 池同一枚举,同样可用。
- `RouteTarget` 不变:池化路由的 target 仍是**池首成员**的名字,所有既有的 target 规则
  (引用、记录、拒绝路径)继续成立;池作为伴随数据存在,与 llm 池同构。

## 3. 结构

| 文件 | 拥有 | 不碰 |
|---|---|---|
| `nico-core/src/config/route.rs` | `UpstreamPool`(成员 + 策略,traffic 域自己的类型)、`PoolStrategy::IpHash` | llm 的 `Pool`/`PoolMember` 语义(weight 0 的含义两域不同,类型不共享) |
| `nico-core/src/config/raw.rs` / `reference.rs` | `upstreams` 池的解析与引用解析(互斥矩阵、0 权重、未声明引用) | 选择行为 |
| `nico-http/src/upstream/pools.rs` | `SelectionTable` 增加 `route_pools`(按 route id):每代发布时从文档的路由构建;池选择器的构建与状态继承 | upstream 成员池的内部 |
| `nico-http/src/proxy/forward.rs` | `select_member` 的两段选择:先路由池、再成员池 | 路由池的构建 |
| `nico-server`(或 nico-http)的 `PoolStrategy → Picker` | traffic 侧的穷尽匹配(新变体必须显式表态) | llm 侧的既有匹配位置 |

`UpstreamState` 增加一个可用性谓词(是否存在可用成员),路由池用它构建候选——这是成员级
"候选列表是唯一健康裁决点"在 upstream 级的直接映射。

## 4. 测试策略

| 层 | 覆盖 | 位置 |
|---|---|---|
| schema/引用 | 互斥、空池、重复、0 权重、未声明引用、strategy-无池、provider 路由写 `upstreams` | `nico-core` 文档校验测试 |
| 分布 | weighted_random 比例(注入 `nico-lb` 的随机源)、round_robin 轮转、ip_hash 粘性与无地址回退 | `nico-http` pools 测试 |
| 可用性 | 某 upstream 全员被剔 → 候选排除;全部不可用 → 502;reload 不归零轮转位置 | 同上 |
| E2E | 池路由的两个 upstream 各挂一个真实 mock,按权重断言双侧都有流量;剔掉其一后流量收敛 | `crates/nico-server/tests/` |
| 不回归 | 无池路由全部既有测试 | 既有套件 |

## 5. 兼容与回滚

- 无池文档行为零变化;带池的文档是纯新增字段(旧二进制以未知字段拒绝——期望方向)。
- 回滚:把池改写回单 `upstream`;无数据迁移。README 的 routes 节与 `strategy` 表随发布更新。

## 6. 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| 灰度作为插件(插件重写目标) | 目标选择是选择层的词汇;插件契约要为它开一个新阶段,而 llm 灰度已以池落地——两个数据面两套机制,契约分裂 |
| 共用 llm 的 `Pool` 类型 | weight 0 的语义两域相反(llm=fallback 候选,traffic=拒绝),文档措辞也不同;类型分开才不会互相长进对方 |
| 池选择不看可用性(盲选) | 单次尝试的 traffic 路由会把 1/N 的流量送向已知全灭的 upstream;候选过滤是成员级既有哲学的延伸 |
| `IpHash` 只给 traffic | `PoolStrategy` 是共享闭集;一个变体两侧都表态,粘性灰度对 llm 池同样成立,拒绝它是人为的 |
| 按请求体/header 分流 | 阶段约束"路由选择不依赖 body";header 分流是路由匹配的另一项能力,另立任务 |
