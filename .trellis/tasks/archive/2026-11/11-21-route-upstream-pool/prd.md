# traffic 路由的 upstream 池(灰度分流)

## Goal

traffic 路由能把请求按权重分布到**多个 upstream**——蓝绿/灰度发布的基本能力。LLM 侧已有
(`routes[].providers` 池,11-11-model-lb),traffic 侧的 `routes[].upstream` 只能指向一个
upstream;本任务把同一套池词汇镜像到 traffic 路由。

## Background

现状:traffic 路由的 `RouteTarget::Upstream(String)` 只有一个名字;upstream 内部的成员池
(`strategy` + endpoints)负责"这台机器还是那台",但**两个 upstream 之间**没有任何分流手段——
把 10% 流量放到新版本,只能拆成两条路由(path 不同才行)或改 upstream 成员。

LLM 侧的形状(要镜像的):`providers` 池替代 `provider` 单目标;`target` 仍取池首成员(所有既有
规则继续成立);`strategy` 只在有池时合法(单成员的策略是一个没人读的值);成员引用在文档校验期
解析,引用未声明的名字是发布错误。

机制取舍(已定):**文档级池,不是插件**。目标选择是选择层的词汇(nico-lb / SelectionTable),
LLM 灰度也是以池而非插件落地的;为它扩展插件契约(插件影响目标选择)会让两个数据面持有两套
灰度机制。

## Requirements

### R1|schema:traffic 路由的 `upstreams` 池

```yaml
routes:
  - id: api-canary
    host: api.example.com
    path_prefix: /v1
    upstreams:                    # 池替代 `upstream` 单目标;两者互斥
      - upstream: api-v1
        weight: 9
      - upstream: api-v2
        weight: 1
    strategy: ip_hash             # 可选;默认 weighted_random;只允许与池同时出现
```

- `upstreams` 与 `upstream` 互斥(与 llm 的 `provider`/`providers` 互斥同构);写了 `provider`
  的路由写 `upstreams` 是校验错误。
- 成员:`upstream`(引用已声明的 upstream)+ `weight`。**weight 0 被拒绝**——traffic 域没有
  fallback 链,"永不选中但占一个名字"是 llm 域的语义(那里有 fallback),这里没有。
- 空池拒绝;重复成员(同名 upstream 出现两次)拒绝;引用未声明的 upstream 拒绝(发布期)。
- `strategy` 扩一个变体:`ip_hash`(调用方完整地址做会合哈希,粘性灰度)加入既有的
  `PoolStrategy`,llm 池同样可用;`weighted_random`(默认,按权重抽样)与 `round_robin` 保持。

### R2|两段选择

- 请求路径:路由池先选 **哪个 upstream**,该 upstream 自己的成员池再选 **哪台机器**。两层都是
  既有的选择词汇,选择器(nico-lb)按策略复用。
- 上游层池是**可用性感知**的:候选 = 有至少一个可用成员的 upstream。整组不可用回答既有的
  `502 upstream_unreachable`,不把请求送向已知全灭的 upstream。
- 池选择器状态(如 round_robin 位置)按代发布、按路由 id 存放;一个请求从它被路由的代中选择,
  与成员表的既有规则一致。

### R3|可观察性与行为保持

- 请求记录与 `gateway_upstream_selected_total` 的标签不变:记录的是**实际服务**的 upstream 与
  endpoint;池只增加"为什么不是别的 upstream"这一层。
- 无池路由的既有行为完全不变(所有既有测试不回归)。

### R4|范围外

- 按请求体内容/header 分流(阶段约束:路由选择不依赖 body);池成员的独立重试/超时(成员级差异
  用多条 upstream 表达);llm 池语义的任何改动(它已工作);跨池 fallback 链(traffic 无重试链)。

## Acceptance Criteria

- [ ] AC1(R1):schema 正负例——互斥、空池、重复成员、0 权重、未声明引用、strategy-无池,各自
      被拒且字段名正确;合法池进入文档。
- [ ] AC2(R2):加权分布比例正确(注入随机源);ip_hash 粘性(同一调用方稳定落同一 upstream);
      round_robin 轮转;可用性感知(某 upstream 全员被剔 → 流量全部落另一 upstream;两者皆灭 →
      502)。
- [ ] AC3(R2):无池路由不回归;池选择器状态按代继承的行为有测试(round_robin 位置不因 reload
      归零,与成员轮转同规则)。
- [ ] AC4(R3):`gateway_upstream_selected_total` 与请求记录在池路由上仍记录实际服务的
      upstream/endpoint。
- [ ] AC5:五条基线门禁 + feature matrix + 文件预算全绿。

## Notes

- 一个交付单元;不做父/子拆分。llm 池的机制与测试是模板,`PoolStrategy` 的 `ip_hash` 变体是
  唯一的双侧新增。
