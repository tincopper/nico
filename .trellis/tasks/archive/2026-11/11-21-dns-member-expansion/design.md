# 设计:upstream 域名成员的 DNS 多成员展开

## 1. 不变量与失败模式

1. **名字→地址集合是发布时快照。** DNS 不是运行期事实:展开在发布管线里做一次,reload 是这份
   快照唯一的更新渠道。请求路径不再有域名查询,发布之后"哪个 IP 是活的"只由 ejection 机器回答。
2. **展开成员是普通成员。** 四元组身份、健康继承、least_conn 计数、weight 选择对展开成员与字面量
   成员完全一致——展开不发明第二种成员,只改变成员集合怎么来。
3. **发布原子性(既有)。** 展开是候选快照构建的一部分:任何 endpoint 解析失败或地址碰撞,整份
   候选作废,LKG 继续服务。展开不会产生"部分生效"的一代。
4. **一台机器一个成员(既有不变量的延伸)。** 文档层拒绝 `(address, port)` 重复;展开把域名还原成
   地址后,同一条不变量在展开结果上继续成立。
5. **内核无 I/O。** 解析是 I/O,发生在 nico-server 的发布管线;nico-core 的文档模型不动。

失败模式与对应行为:

| 失败 | 行为 |
|---|---|
| 发布时域名解析失败 / 解析为空 | 发布失败,点名 `upstreams[i].endpoints[j].address`;LKG 保持;启动期拒绝启动 |
| 展开地址与同 upstream 另一成员 `(address, port)` 碰撞 | 发布失败,点名引起碰撞的字段;LKG 保持 |
| 运行期某 IP 失联 | 既有 ejection:该 IP 的成员被 eject,流量按选择规则落到其余成员 |
| 运行期域名的地址集合变化 | 下一次发布生效;消失的 IP 随成员一起退出(继承按精确四元组匹配,不猜测) |

## 2. 契约

Schema 零变化。`address` 本就与 `upstreams[].host` 共用同一条 `validate_host` 规则,域名是合法的
成员地址;变化的是域名成员在**运行期**的形状——从"一个以域名为 address 的成员"变成"若干以 IP 为
address 的成员"。

对外可观察的变化(README 与 spec 落字):

- 域名成员的流量份额 ∝ 解析地址数 × weight(此前整个域名是一份)。
- 日志与指标里成员由 `(name, address)` 联合区分,展开后同一 endpoint 名下出现多行,`address` 是
  具体 IP。
- 请求路径不再有每请求 DNS;解析失败从"请求失败"变为"发布失败"。
- LLM provider 的 destination 不展开(每请求首个解析地址),是记录在案的边界而非遗漏。

## 3. 结构

### 3.1 展开的归属:nico-http 的成员构建,nico-server 的时机

解析是 I/O,而"成员集合怎么来"是 nico-http 的词汇(它拥有 `Destination`、`resolve`、
`UpstreamPools`、`MemberId`)。因此:

- **nico-http** 新增展开步骤(位于 `upstream/` 下,与 `pools` 同级):输入一份 `Document`,输出
  展开后的成员视图——`async`,内部用 `tokio::net::lookup_host`,对 IP 字面量短路。失败返回带
  `(upstream, endpoint 字段)` 的类型化错误。展开逻辑(域名→多地址、碰撞检查、weight/name 继承)
  与解析结果注入解耦,纯逻辑部分无网络可测。
- **`UpstreamPools::publish`** 的输入从 `&Document` 变为 `&Document` + 展开视图:构建
  `SelectionTable` 时,域名 endpoint 被它的地址集合替换。`publish` 本身保持同步——展开在其之前
  完成并作为参数传入,快照构建的原子性不变。
- **nico-server** 的 reload 管线(`publish` 之前)await 这个展开调用,失败走既有的候选失败路径
  (keep LKG、日志、single-flight 不受影响)。

这样分工的理由:发布管线的节奏(何时解析、失败后怎么办)是 nico-server 的事;成员集合的形状、
身份与验证是 nico-http 的事。nico-core 不出现在任何一环。

### 3.2 身份、权重与名字

- 身份沿用 `(upstream 名, member 名, address, port)`:展开成员的 address 是 IP,同一 IP 跨发布
  精确匹配并继承;IP 消失则成员退出,不猜测对应关系。
- weight:每个展开成员继承 endpoint 的 weight。多 IP 域名的流量份额 ∝ 地址数 × weight,与 nginx
  一致;均分会让"权重"在 least_conn 与轮转下的语义含混。
- 名字:保持 operator 写的名字。`name` 的文档语义是"角色,不是机器"(`address:port` 会随换机而
  变),DNS 展开的正是"角色下的机器集合"——名字不变,机器由 address 区分。合成名(`web[2]`)会让
  日志与指标出现 operator 从未写过的名字,否决。

### 3.3 请求路径

展开后成员地址全为具体地址,`upstream::resolve` 对 IP 字面量走 `lookup_host` 的本地短路路径,不再
产生网络查询。`resolve` 保持原样(它仍是"把 Destination 变成可连地址"的那一步),只是它的输入在
发布后不再是域名——provider 的 destination 除外,那是 R4 的记录在案的边界。

## 4. 测试策略

| 层 | 覆盖 | 位置 |
|---|---|---|
| 展开(纯逻辑,注入解析结果) | N 地址 → N 成员;失败/空 → 点名字段的错误;字面量原样;weight/name 保留;碰撞拒绝 | `nico-http` upstream 测试 |
| 选择与继承 | 同名多 IP 是不同成员;同 IP 跨发布继承健康;IP 消失随成员退出 | `pools` 测试 |
| 发布管线 | 解析失败 → 候选作废 keep LKG;启动期拒绝启动 | `nico-server` reload/启动测试 |
| E2E | 域名展开成两成员,其一失联 → 流量落另一成员 | `crates/nico-server/tests/` |
| 请求路径 | 成员地址为字面量,无每请求域名查询 | upstream 测试 |

解析注入:展开函数把 `lookup_host` 收敛为一个可替换的异步解析闭包/参数,生产路径传真实解析,
测试传注入结果——不引入 resolver trait 的注册表,一个参数就够。

## 5. 兼容与回滚

- 行为变化对既有部署是可感知的(域名成员从"每请求首 IP"变为"发布期展开逐 IP"),README 的
  `config_source`/`upstreams` 相关节与 nginx 迁移对照同步更新;无 schema 迁移。
- 回滚:把域名成员改写为 IP 字面量即回到旧语义;无数据迁移。

## 6. 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| 请求期展开(每请求解析全部地址) | 每请求 N 次查询或需要进程内缓存;成员身份随 DNS 抖动,健康无从谈起 |
| 后台 TTL re-resolution(nginx resolver 形状) | 引入本仓库第一个周期性后台任务及其生命周期/取消/发布原子性问题;DNS 变更靠 reload 的既有立场够用;等真实需求再立任务 |
| 在 nico-core 展开 | 内核无 I/O 是硬规则;文档模型也不该有两种成员形状 |
| 合成成员名(`name[2]`) | name 是角色不是机器;四元组本就可区分,合成名污染 operator 没写过的日志与指标 |
| 展开成员均分 weight | 与"weight 是 endpoint 的属性"冲突;nginx 的逐地址继承是已被验证的语义 |
| 碰撞时允许重复成员 | "一台机器一个成员"是文档层已写下的不变量(重复会让失败分裂到两个计数器);展开不豁免它 |
