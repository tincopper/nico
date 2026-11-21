# upstream 域名成员的 DNS 多成员展开

## Goal

`upstreams[].endpoints[].address` 写域名时,把发布期解析出的每个地址展开成独立的池成员:逐 IP 的
健康判定、least_conn 计数与 failover。同时把 DNS 从请求路径上摘掉——发布之后的请求不再做域名查询。

## Background

现状:`nico-http/src/upstream.rs` 的 `resolve()` 在每笔请求上 `lookup_host(...)` 后取 `.next()`
首个地址。文档模型本就支持多成员(`Endpoint` 的 name/address/port/weight,与 upstream 的逻辑 host
分离),池按四元组 `(upstream 名, member 名, address, port)` 继承观察状态——但域名成员参与身份
四元组的 address 是**域名本身**,于是:

1. **同名多 IP 不展开**:解析结果里的第二个 IP 永远收不到流量;健康检查、least_conn、failover
   都感知不到它。
2. **健康误归因**:DNS 轮询下,多台机器的成败都记在一个成员名下——一台坏机器把整个域名 eject,
   或者首个 IP 宕机让每笔请求都失败,而 DNS 本可以给出健康的候选。
3. **请求路径上的 DNS**:每请求一次查询;解析器的延迟与抖动直接进入请求路径,成为请求失败面。

11-18 nginx 迁移对照把该缺口记入 session journal 的 Next Steps。nginx 对域名 server 的语义就是
"展开成逐地址的 upstream 条目,逐地址健康"。

## Requirements

### R1|发布期展开

- 发布管线对 `address` 为域名的 endpoint 解析一次:每个解析出的地址成为一个独立成员;IP 字面量
  成员原样保留。
- 展开成员沿用既有四元组身份 `(upstream 名, member 名, ip, port)`:同名多 IP 天然是不同成员;
  重新发布时,同一个 IP 继承它上一代的观察状态(继承规则是既有的,不因展开而改变)。
- weight:每个展开成员继承 endpoint 的 weight——流量份额 ∝ 解析地址数 × weight,与 nginx 对域名
  server 的处理一致。
- member 名保持 operator 写的名字,不造合成名;成员由 `(name, address)` 联合区分,符合既有
  "name 是角色、address 才是机器"的文档语义。

### R2|解析失败的处置

- 发布时某域名解析失败(或解析为空):该次发布失败,错误点名 `upstreams[i].endpoints[j].address`;
  LKG 继续服务;启动期同样拒绝启动。与既有不变量一致:一份被发布的文档不携带本构建服务不了的形状。
- 展开后的地址若与同 upstream 另一成员的 `(address, port)` 重复(域名指向了一个字面量成员、或两个
  域名指向同一台机器),与文档层"不得重复另一成员的 address 和 port"同一条不变量:拒绝发布并点名
  字段。

### R3|请求路径不再解析

- 发布后成员地址全为具体地址;upstream 选择路径不再有每请求的域名查询。字面量解析保留为本地常量
  操作(`lookup_host` 对 IP 字面量不产生网络查询)。

### R4|边界:LLM provider 不展开

- LLM provider 的 destination(`base_url`)不经 upstream 池,保持现语义(每请求首个解析地址);
  为 provider 做逐 IP 池是另一个能力,文档写明这条边界。

### R5|范围外

- 周期性 re-resolution / DNS TTL 后台刷新:名字→地址集合是发布时快照,DNS 变更靠 reload 生效
  (配置驱动的既有立场);不引入第三方 resolver(hickory 等);SRV 记录;unix socket 成员;
  逐 IP 权重差异化。

## Acceptance Criteria

- [ ] AC1(R1):域名成员按注入的解析结果展开为 N 成员;展开逻辑无网络可测;weight/name 保留;
      IP 字面量成员原样。
- [ ] AC2(R2):解析失败/空结果拒绝发布并点名 `upstreams[i].endpoints[j].address`,LKG 保持,
      启动期拒绝启动;展开地址与字面量成员撞 `(address, port)` 同样拒绝。
- [ ] AC3(R1/R2):同名多 IP 是不同成员;同一 IP 跨发布继承健康状态。
- [ ] AC4(R3):upstream 请求路径无每请求域名查询(以字面量短路钉住);既有 upstream 测试不回归。
- [ ] AC5(R4):provider 边界在 spec/README 落字。
- [ ] AC6:五条基线门禁(fmt/clippy/test/doc/deny)+ feature matrix + 文件预算全绿。

## Notes

- 一个交付单元;不做父/子拆分。
