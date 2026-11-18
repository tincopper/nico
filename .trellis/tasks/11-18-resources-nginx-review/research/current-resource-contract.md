# 当前资源配置契约：代码与测试取证

## 0. 范围、方法与置信度

本文件是 `resources.yaml` nginx 对齐**规划审阅输入**，不决定 schema 或运行行为变更，不将语义差异或文档失真自动判定为产品 bug。

- 本文件由规划阶段的研究子代理针对 `.trellis/tasks/11-18-resources-nginx-review/` 编写。子代理的会话查询未取得主会话任务指针；调研按派发时的显式任务路径进行。主会话任务状态与实际验证以 `task.json` 和 `verification.md` 为准。
- 研究子代理读取本任务 `prd.md`、`.trellis/spec/backend/index.md`、`database-guidelines.md`、`pingora-guidelines.md`、README、示例资源配置；逐文件读取下文引用的配置模型、校验、编译、请求选择与关键测试。**研究子代理未执行测试**，下文“测试覆盖”指已核对测试源码和断言；主会话已执行的 829 项库测试记录在 `verification.md`，不包含 server 集成测试。
- nginx 官方资料及上个任务分析由主代理另行核实，本文件不重复外部对照；不穷尽 LLM codec。
- 证据标识：**[代码+测试]** 有实现与直接断言；**[代码]** 调用链明确，但未找到/未阅读直接场景测试；**[未验证]** 不据此下运行保证。
- 所有路径均相对仓库根；`文件:行号` 指本次工作区。

## 1. 先说边界：三组概念只能近似对应

| 资源 | 当前真实职责 | 证据与边界 |
|---|---|---|
| `routes` | 平面、独立的请求匹配规则：可选 exact host + byte path prefix + 一个目标 + 路由策略引用/预算 | `crates/nico-core/src/config/raw.rs:136-194`、`crates/nico-http/src/route.rs:63-98`。没有 server/location 层级、listener 绑定、method matcher、regex/exact-path 字段；这是当前 schema/选择函数的完整形状，不是对未来能力的决定。 |
| `upstreams` | 命名 HTTP 服务：逻辑 host/传输 + endpoint 成员 + 成员选择/被动健康 | `crates/nico-core/src/config/upstream.rs:3-16`、`crates/nico-http/src/upstream/pools.rs:259-301`。不是 URL/URI 模板，也不是 LLM provider。 |
| `plugins` | 命名策略**实例定义**；`kind` 通过注册表解析为实现 | `crates/nico-core/src/config/plugin.rs:1-18`、`crates/nico-plugins/src/compile.rs:213-256`。`global_plugins` 与 `routes[].plugins` 才决定使用范围；“定义在顶层”不等于“全局生效”。生产内置三种 kind，不是任意 nginx 模块装载语法。 |

**listener 与资源表的关系 [代码+局部测试]**：`crates/nico-http/src/service.rs:98-121` 只构建一个 `HttpApp::new(state)`，再把全部 listener 注册到同一 service；`crates/nico-server/src/server.rs:104-108,156-181` 将同一状态交给 reload 与 HTTP service。路由选择只拿请求头和同一 document 的 routes，不拿 listener id（`route.rs:63`）。`crates/nico-http/src/service/tests.rs:195-210,237-262` 覆盖多 listener 注册和明文/TLS 并存。**未独立重验相同 host/path 经不同 listener 的整套路由/插件矩阵**；不能据此凭空增加 server schema。

## 2. 路由匹配、冲突、目标与预算

### 2.1 host/path 的实际选择规则

1. **host 是 exact、ASCII 大小写不敏感比较；配置里不带端口，入站 host 比较前去端口。**未声明 host 才是任意 host。实现：`crates/nico-http/src/route.rs:63-98,112-138`、`crates/nico-core/src/config/host.rs:72-112`。测试：`crates/nico-http/src/route/tests.rs:96-151` 覆盖其他 host 不命中、大小写/端口及目标 authority；`crates/nico-server/tests/http_routing.rs:108-129` 有真实服务断言。**[代码+测试]**
   - authority 来源依次为 URI authority → HTTP/1 absolute-form raw target authority → Host header，非 UTF-8 来源匹配不到限定 host（`route.rs:112-122`）。`route/tests.rs:184-209` 的 `an_absolute_form_target_wins_over_the_host_header` 验证 absolute-form 与 Host 冲突时 target 胜出。
   - host 校验拒绝空、>253 bytes、`://`、冒号（含 IPv6 literal）、空白；**并非完整 DNS 语法校验**（`host.rs:72-88`）。没有 wildcard/regex 解释；即便某特殊字符串通过形状校验，运行时也是字面值比较。尾点不归一化：`api.example` 与 `api.example.` 不同，`host.rs:155-175` 有断言。wildcard 字面配置场景**未直接测试**。
2. **在真正匹配 path 的候选里，host-specific 优先，再最长前缀；无显式 priority 数值。**限定 host 的短 `/` 可以胜过不限 host 的 `/api/v1/`。实现：`route.rs:68-95`；单测 `route/tests.rs:19-104`；E2E `http_routing.rs:96-129`。**[代码+测试]**
   - 这不是先选 host 组后绝不回退：若某 host 的限定规则没有一个 path 命中，不限 host 的规则仍是候选（由同一循环可见；本轮未找到该特定负例的直接 E2E）。
3. **path 按 `header.uri.path()` 的字节前缀匹配，不按段、不读 body，不把 query 当匹配条件。**`/api` 命中 `/apix`，`/api/` 不命中 `/api`。实现 `route.rs:65-81`；单测 `route/tests.rs:40-66`；请求头在 body 到来前已送达 upstream 的实测断言在 `crates/nico-server/tests/http_body.rs:134-189`。**[代码+测试]**
   - nico 的 matcher 自己不做 URI decode、斜杠折叠或 dot-segment 归一化；不能把此前缀理解为规范化后的 location。**入站所有特殊 URI 经 Pingora parser 后的完整字节矩阵未重验**。
   - 配置前缀必须 `/` 开头、≤256 bytes、无空白，而且拒绝**任意 `..` 子串**，不只是父目录段（`crates/nico-core/src/config/semantic.rs:320-337`）。`semantic/tests/paths.rs:4-71` 覆盖 rooted、父段、空白与长度；对非父段中的 `..` 没有本轮读到的直接测试。

### 2.2 重复与冲突不是“后写覆盖”

- route id 全文档唯一；upstream/provider/plugin 名在各自集合唯一；所有目标与插件引用必须存在。实现 `crates/nico-core/src/config/reference.rs:70-88`；测试 `reference/tests.rs:179-307,327-379`。**[代码+测试]**
- 同一 `(ASCII-lowercase host 或 None, 原样 path_prefix)` 重复，整份文档拒绝，不管 id/target/plugin 是否不同。实现 `semantic.rs:232-275`，比较/去重共用 `host.rs:99-111`；测试 `semantic/tests/matches.rs:4-87` 包括同 host 不同大小写。**[代码+测试]**
- 重叠但不相等的前缀合法，host-specific 与 host-agnostic 同前缀也合法，按上述排序选一条，不合并另一条的插件或限制。嵌套前缀和同前缀不同 host 等级直接由 `http_routing.rs:77-129` 证明。**[代码+测试]**
- README `:249` 与 router `:15` 写“同等级同长度，文档靠前胜出”，代码用 `>` 保留 incumbent；但对于**正常解析的整份 document**，同一请求若同 host 等级、同长度前缀都匹配，两前缀必相等，而重复已被拒绝。`route/tests.rs:154-175` 注释明确说明 tie-break 对合法文档不可达。不能教用户用列表顺序解决相同匹配规则。**[代码+测试说明]**

### 2.3 target / protocol / mode 分工

- 配置恰选 `upstream` / 单 `provider` / `providers` pool 三种写法之一；缺失或并存拒绝。pool 非空，成员存在且不重复；route 的内部 `RouteTarget::Provider` 在 pool 形态记录第一个成员，真正分发仍看 pool。实现 `reference.rs:164-200,273-304`；测试 `reference/tests.rs:24-117`、`reference/tests/pool.rs` 中 `a_route_naming_a_provider_and_a_pool_is_rejected`、`a_route_naming_an_upstream_and_a_pool_is_rejected`、`a_pool_route_keeps_its_first_member_as_the_route_target`。**[代码+测试]**
- `protocol` 是 LLM **客户端方言**，不是 HTTP/1.1、HTTP/2 或 upstream 传输；provider route 必填，upstream route 禁止。`mode` 也是 provider route 字段：默认 convert，或 passthrough。`reference.rs:324-365`、`route.rs:46-60,249-265`；测试 `reference/tests.rs:93-150`、`document/tests/llm.rs:35-68`。**[代码+测试]**
- upstream route 还拒绝 `max_response_bytes`、`retry_budget`、`fallback`、`ttft_timeout_ms`、`token_budget`；route 的 `strategy` 仅在 provider pool 上有效，traffic 的策略放在 upstream。实现 `reference.rs:174-187,324-365`；测试 `reference/tests/fallback.rs` 的 upstream 字段拒绝、`reference/tests/pool.rs` 的 strategy/token-budget 负例、`document/tests/attempts.rs`。**[代码+测试]**
- codec/provider kind 的存在和 passthrough 两侧方言相同，是**发布编译**才判断，不是 core 的字符串形状校验。`crates/nico-server/src/llm/plan.rs:267-310`；测试 `src/llm/tests/plan.rs:79-220`、`:312-403`。passthrough 也会使用 provider 编译出的 path/headers，并非整个 HTTP 请求完全不动，见 §3.2。
- passthrough 的重试/fallback 拒绝是**有效值**判断：`plan.rs:405-406` 仅在 `retry_budget > 1` 或非空 fallback 时拒绝，显式 `retry_budget: 1` / `fallback: []` 等同默认。`src/llm/tests/plan/chain.rs:125-153` 覆盖非默认拒绝；显式默认组合未直接验证。不能把“字段一出现就拒绝”当作现有实现事实。
- traffic-only 构建可声明但不引用 provider；有 provider route 就拒绝发布。实现 `crates/nico-server/src/reload/llm.rs:137-156`，测试 `crates/nico-server/tests/role_traffic.rs:48-99`。**[代码+测试]**

### 2.4 request 继承：哪些被拒绝，哪些被截断

| 字段 | 解析/校验 | 请求时生效 | 直接测试 |
|---|---|---|---|
| `request.max_body_bytes` | 必填 `u32`，0 合法，u32 是数值范围 | 没 route override 时继承；0 仅允许空 body | `crates/nico-core/src/config/request.rs:88-100`；`crates/nico-http/src/context/tests.rs:222-234` |
| route `max_body_bytes` | 可选；大于 document **拒绝** | `min(route, document)` | `semantic/tests/numeric.rs:81-121`；`nico-http/src/budget/tests.rs:32-48` |
| `request.timeout_ms` | 必填，1..=600000 | request_filter 开始记录一次 started/deadline | `request.rs:73-80`；`nico-http/src/context/tests.rs:90-113` |
| route `timeout_ms` | 可选，1..=600000；大于 document **仍可解析** | `min(route, document)`，从原 started 算，不从路由命中重新计时 | `nico-http/src/budget/tests.rs:90-119`；`nico-core/src/config/document/tests.rs:69-143` |
| `request.max_response_bytes` | 可选，默认 8 MiB；`u32`、0 合法 | **convert 的整包响应**上限；不是普通代理全响应流量上限 | `request.rs:103-112`；`nico-llm/tests/route/transform.rs:210-273` |
| route `max_response_bytes` | 仅 provider route 可写；高于 document **拒绝** | 缺席继承；运行时取 min；passthrough 无转换缓冲，实际上不消费此上限 | `semantic/tests/numeric.rs:123-173`；`nico-http/src/budget/tests.rs:121-142`；`proxy/hooks/llm.rs:223-257` |

消费入口：`crates/nico-http/src/context.rs:115-155`、`budget.rs:14-36,80-95`。限制是 document→命中 route 的两级收紧，不是沿路径层级继承；没命中的“父前缀 route”不提供默认插件/限制。

**特别核实示例 30000 / 120000 [代码+测试]**：`config/resources.yaml:17` 写全局 30000ms、`:175` 写 llm-chat 120000ms。`semantic.rs:242-244` 不比较两者，因此文档合法；`budget.rs:80-85` 决定实际总预算仍是 **30000ms**。`budget/tests.rs:109-119` 的 `a_route_budget_above_the_document_budget_is_clamped_to_it` 专门钉住该行为；`document/tests.rs:69-143` 验证同样 30000/120000 形状可解析。README `:239` 的“受 request.timeout_ms 约束”若指**生效预算**正确，若被理解为“写更大会报错”则不对；示例不能据此提供两分钟预算。先归为示例/说明可能误导，不据此认定 runtime bug。

**限制的执行时机 [代码+测试]**：global request chain 已运行、路由命中后，声明长度超限在 route request chain 前拒绝（`proxy/hooks/request.rs:109-171`）；未声明长度按客户端 body chunk 累计（`proxy/hooks/body.rs:51-64`），流式超限可能已有部分 body 发到上游。`http_limits.rs:84-136` 分别断言 413 前不连上游与 chunked 中途停止；`:138-208` 验证无响应 504、head 已提交后截断；`:211-255` 验证持续慢上传 408。插件异步 stage 被剩余 deadline 包裹（`plugin/chain.rs:456-467`；`plugin/chain/tests/bytes.rs:126-167`）。总预算不是每一阶段重新赠送的 idle 窗口；**未验证每一种零字节停滞/DNS/写阻塞都恰在 deadline 时立即被独立计时器终止**。

**`ttft_timeout_ms` 命名边界**：范围 1..=300000（`semantic.rs:245-250`），连接仍用剩余 request budget，读用 min(ttft, remaining)（`proxy/hooks/request.rs:239-250`、`upstream.rs:201-208`）。实际是 Pingora **每次 read** 的超时，流式 chunk 间隔和非流式读取也受影响，不只“首 token”；README `:546` 起已说明，模型部分注释仍写 streaming/first-byte。`upstream/tests.rs:110-145` 验证两种 budget 分离/取小；本轮未重复读所有 pool 的真实超时场景。

## 3. upstream：名字、逻辑身份、连接目标及健康

### 3.1 三种名字/地址，不宜混用

- `name` 是 routes 引用的集合键，不自动成为 wire Host；`host` 是逻辑 HTTP/TLS 名；endpoint `address:port` 才决定连接。`scheme/host/port` 在 upstream 必填；endpoint 没有 scheme，address/port 缺席继承 host/port，weight 默认 1。不写 endpoints 展开成名为 `default` 的单成员；显式 `[]` 拒绝。实现 `crates/nico-core/src/config/raw.rs:90-132`、`validate.rs:131-175`、`semantic/upstreams.rs:19-26,60-97`。测试 `config/upstream/tests.rs:38-83` 覆盖展开/成员 port/weight 默认与逻辑 host 不变。**[代码+测试；显式空列表负例仅核实代码]**
- 逻辑 host 与 address 都用同一 host 形状校验，不能携带 scheme/port/IPv6 literal；port 为 u16 且非 0。endpoint name 同 upstream 内唯一，`(address 字符串, port)` 不得重复，weight 1..=1000。`semantic/upstreams.rs:67-95`。**[代码]**：本轮没有找到这些 endpoint 负例的专门测试，不能笼统称已全覆盖。
- endpoint 地址去重是**原字符串**，不是 DNS 解析后的 socket 地址，也没有 host_key 小写归一化（`:68,78`）；因此“同一物理机器不可写两遍”不是 validator 能完整证明的事实。upstream host 非完整 DNS 校验同 §2.1。
- DNS 在请求构建 peer 时 `tokio::net::lookup_host((address,port))`，仅取第一个解析结果（`crates/nico-http/src/upstream.rs:147-153,176-181`）；一个 hostname 的多个 DNS 地址**不会自动成为独立 endpoint/LB 成员**。字面 IP/逻辑 authority 分离由 `upstream/tests.rs:159-189` 验证；真实 DNS 更新/多地址顺序本轮未测试。

### 3.2 端口、path、header 的精确行为

1. 选中成员生成 `Destination::new(upstream.scheme, endpoint.address, endpoint.port).with_authority(upstream.host)`（`upstream/pools.rs:292-301`）。SNI 取逻辑 host，不带 port；Host 取**逻辑 host + 选中 endpoint 的实际非默认端口**，http:80/https:443 才省略。实现 `upstream.rs:163-168,196-208,223-228`。测试 `upstream/tests.rs:86-98,192-214`；E2E `crates/nico-server/tests/upstream_lb.rs:317-321,333-367`。**[代码+测试]**
   - 所以 `Endpoint` 注释“port 不参与 Host”（`nico-core/src/config/upstream.rs:141-143`）若理解为完整 authority，措辞不准确；不变的是 hostname，不是 authority 的端口。
2. **普通 upstream 无 path 字段，路由前缀不自动剥离/替换。**模型明言 path 原样转发（`nico-core/src/config/upstream.rs:15-16`）；HTTP head hook `proxy/hooks/request.rs:328-400` 对普通 route 不设置 target，`proxy/hooks/llm.rs:50-51` 直接退出。**[代码]**：已读测试证明 ordinary HTTP 正常转发，但未读到专门比较 `/prefix/x?query` 原样到 origin 的端到端断言；特殊 target 的框架规范化不在本轮保证范围。
3. **provider passthrough 不等于 upstream 原样代理。**convert 与 passthrough 都把目标 path 改成 adapter 编译值并覆盖 provider headers（`proxy/hooks/llm.rs:56-98`）；convert 另移除调用方 authorization/x-api-key/x-goog-api-key、accept-encoding，拒绝非 identity content-encoding。passthrough 不解码 body、保持 framing，但 provider 的 credential/header 仍能覆盖调用方的。`crates/nico-server/tests/llm_passthrough.rs:116-147` 同时断言 path、body 不变、Host 之外的 provider authorization/content-type 覆盖；`llm_convert.rs:171-199` 断言 convert path/key/body。**[代码+测试]**
4. 所有转发在 provider head 改写后统一：Host 改写为 destination；X-Forwarded-Host 取客户端 Host（可转 UTF-8 才写）；X-Forwarded-For 保留已有值并追加实际 peer IP；X-Forwarded-Proto **已有就不覆盖**，缺席才按此连接 TLS 与否生成。实现 `proxy/hooks/request.rs:343-380`、`proxy/forward.rs:101-105,229-243`。普通 HTTP E2E `http_body.rs:170-189`；XFF 单测 `nico-http/src/proxy/tests.rs:305-340`。**[代码+测试]**
   - 限流和 ip_hash 不读取这些 forwarded header 来信任“原客户端”，读的是 socket peer（`forward.rs:101-105`、`proxy/hooks/request.rs:69-89`）。本轮未扩展成 forwarded 信任安全审计。

### 3.3 负载均衡：策略名字不代表与 nginx 算法等价

| 策略 | 已核实行为 | 实现 / 测试 |
|---|---|---|
| `round_robin`（默认） | 按当前可用 candidates 顺序轮转；不读 weight | `crates/nico-lb/src/picker.rs:164-174`；`nico-http/src/upstream/pools/tests.rs:173-183` |
| `weighted_round_robin` | smooth weighted；只算可用 candidates 的权重 | `nico-lb/src/picker.rs:204-235`；`nico-lb/src/picker/tests.rs:93-124`、`nico-http/src/upstream/pools/tests.rs:186-199` |
| `least_conn` | 比较正在使用该 member 的**请求数**，平票轮转；不是 Pingora idle socket 数；不读 weight | `nico-lb/src/picker.rs:284-298`；`nico-http/src/upstream/pools/member.rs:154-166,214-219`；`pools/tests.rs:203-235` |
| `ip_hash` | 完整 peer IP bytes 与 **endpoint name** 做 FNV-1a rendezvous score，最大值胜出；不读 weight；无 peer IP 时轮转 | `nico-lb/src/weighted.rs:32-57,148-170`；`weighted/tests.rs:101-184`、`nico-http/src/upstream/pools/tests.rs:238-272` |

映射枚举至 picker 仅一处：`nico-http/src/upstream/pools/picker.rs:18-24`。因此写了 endpoint weight 不等于任何策略都按权重运行；weight=0 在 traffic endpoint 被拒，不是 backup/down。`ip_hash` 的键含 endpoint 名，改名会改变映射；新增成员只搬迁被新成员赢得的键有直接测试 `nico-lb/src/weighted/tests.rs:116-140`，但“必定约 1/n 均匀迁移”的统计保证未由该测试证明。

### 3.4 被动健康、重载与本次请求

- 默认 max_fails=3、fail_timeout_ms=30000；max_fails 可 0..=1000，0 禁止新失败触发剔除；cooldown 1..=600000。`nico-core/src/config/upstream.rs:32-44`、`semantic/upstreams.rs:35-48`。**[代码]**，0 在最初即禁用的行为有 `nico-http/src/upstream/pools/tests/health.rs:290-311`。
- 连续失败而非时间窗内累计：连接/DNS/握手失败、connect/handshake timeout、origin 5xx 计失败；最终非 5xx 响应（含 4xx）清零。read/write timeout、已收到 head 后的 body 中断不在 traffic 成员三种计数点里。实现 `proxy/hooks/request.rs:218-226`、`proxy/hooks/response.rs:62-68,110-115,223-239`；分类测试 `proxy/tests/member.rs:90-110`。**[代码+测试；非计数路径由完整 hook 核对]**
- 达阈值剔除，cooldown 后一个**真实流量请求**获 probe，不是后台主动请求；probe 成功回归、失败重新冷却；probe 无结果再过一个 cooldown 可再试。晚到非 5xx 应答可提前回归。实现 `upstream/pools/health.rs:112-170,212-244`；测试 `pools/tests/health.rs:77-287` 完整覆盖关键转移。**[代码+测试]**
- 全部无可用成员直接 `502 upstream_unreachable`，不强选一个坏成员；没有本次请求跨 traffic endpoints 自动 failover。`proxy/forward.rs:174-210`；`context/attempt.rs:57-71` 要 LLM attempt 才能 retry；测试 `proxy/tests/member.rs:114-130`、E2E `upstream_lb.rs:184-220`（撞死成员那次返回 502，之后请求走 survivor）。**[代码+测试]**
- 成员健康/活跃数按 `(upstream name, endpoint name, address, port)` 继承；scheme、逻辑 host、weight、健康参数不是此 identity 一部分。新 generation 保留自己的列表和参数，计数可能与旧 generation 请求共享。`upstream/pools.rs:75-91,189-219`、`pools/member.rs:39-52`。相同身份延续/改地址归零/轮转延续由 `pools/tests.rs:276-385` 覆盖，健康阈值重调于下次 failure 判断由 `pools/tests/health.rs:329-361` 覆盖，真实 cooldown reload `upstream_lb.rs:287-330`。**[代码+测试]**
- 不把旧注释扩展成未证保证：健康机 available/admit 没有“max_fails=0 立即清掉已有 ejection”的 special case（`health.rs:112-131`）；从已剔除状态热改 0 的行为本轮未测。并发旧请求/新 probe 结果归属也未扩展审计。

## 4. plugins / global_plugins：实例、组合顺序、enabled、body 与认证限流

### 4.1 定义与使用点

- `plugins[].name` 定义实例，`kind` 定义实现类别，config 由 factory 解成强类型。一个 kind 可有多个不同参数实例；不能在 route 引用处 override 参数或单次 disable（完整 RawRoute 仅 `Vec<String>`，`raw.rs:183-194`）。生产注册表目前只有 jwt/api_key/rate_limit：`crates/nico-plugins/src/lib.rs:64-79`。未知 kind 的**启用实例**拒绝发布；WASM 是后续 seam，不是已安装能力。测试 `compile/tests.rs:300-331`。**[代码+测试]**
- 声明但未引用不会自动运行，然而**所有 enabled 定义都会编译**，unused 的坏 kind/config/凭证也会导致文档编译失败（`compile.rs:170-186` 无 used 过滤）。route/plugin 列表缺席的默认由 `reference.rs:215`、`raw.rs:32-33` 决定。测试 `compile/tests/plan.rs:370-393` 明确“声明不等于全局执行”；unused 坏定义没有本轮读到的独立负例。provider 与此不同：无 route 引用的 provider 不物化凭证（`src/llm/plan.rs:142-155`、`src/llm/tests/plan.rs:407-432`）。**[代码+部分测试]**
- 每一 scope 列表内重复引用拒绝、unknown reference 拒绝，但同名实例**可同时出现在 global 与 route**，共享 Arc 实例而非跨 scope 去重。`reference.rs:100-117,215-228`；`compile.rs:340-364,369-387`；测试 `compile/tests/plan.rs:329-366` 明确同一实例共享，`nico-core/src/config/document/tests/plugins.rs:243-296` 覆盖 global 引用错误。请求链两次被调用由 hook 顺序直接推出，未读到同名同时使用的“精确两次执行”E2E。**[代码+共享实例测试]**

### 4.2 实际组合顺序

```text
请求头：global 的 Authenticate → Limit → Other
      → 路由匹配（使用 global 改写后的头/URI）
      → route 的 Authenticate → Limit → Other
普通请求 body：global chain → route chain（每个 scope 内仍按已编译顺序）
普通上游响应头/body：route chain → global chain（scope 反向，但 scope 内仍正序）
日志：route chain → global chain
```

证据：`crates/nico-plugins/src/compile.rs:328-331,450-471`（stable sort，同类别保留**使用点列表**顺序，不是定义顺序）；`crates/nico-http/src/proxy/hooks/request.rs:109-175`；`body.rs:125-154,228-243`；`response.rs:88-97,329-334`；`plugin/chain.rs:265-278,332-347,419-441`。测试 `nico-plugins/src/compile/tests/plan.rs:31-56,120-175`、`nico-server/tests/plugins_global.rs:80-142`、`nico-http/src/plugin/chain/tests/control.rs:14-80,165-187` 与 `bytes.rs:61-123`。**[代码+测试]**

- “auth before limit”只在**同一 scope**排序成立：global IP limit 会先于 route auth；不会把两个 scope 合成一条全局排序链。
- global 可改 URI/Host，matcher 看改后值；route 插件在 match 后改 URI只改变转发 target，不重新 match（`request.rs:122-129,171`；`plugin/context/contract.rs:50-59`）。能力 seam 有 `plugin/context/tests.rs:73-109` 单测；生产三种内置不实现 rewrite；未重验自定义插件对 route 重新选择的 E2E。
- first refusal/Respond 停止后续请求 stage，不是可选 OR 认证；两个认证都列入链，就都必须通过，第一身份保留，后面不能替换。`plugin/chain.rs:265-278`、`context.rs:444-448`；测试 `plugin/chain/tests/control.rs:35-80`、`plugin/context/tests.rs:356-377`。**[代码+测试]**

### 4.3 enabled 不是“仅停这个 route”

默认 enabled=true；false 保留声明与合法引用，但**跳过 registry lookup、kind config 编译、密钥读取和运行**，对其全部使用点生效。外层 schema/name/kind 非空/error_response 形状仍先由 core 校验。实现 `raw.rs:221-246`、`plugin.rs:136-162`、`semantic/plugins.rs:27-31`、`compile.rs:213-225,457-471`。仅 disabled 引用的 route 得到空 chain，而非被当成漏编译 fail-closed。测试 `compile/tests/plan.rs:59-117` 同时证明不存在的 secret 不读取、disabled unknown kind 可接受。**[代码+测试]**

### 4.4 body buffering 的准确范围

- 一个 **enabled 且被使用**实例 buffers_body=true，即使它本身只有 header hook，也把相应 route 的**所有**请求体插件放到 whole-body 模式；global buffered 实例会标记全部 route。非使用/disabled 不贡献。实现 `compile.rs:359-375,401-403`；测试 `compile/tests/plan.rs:252-325`。不存在一条请求链里部分插件流式、部分 whole 的模式。**[代码+测试]**
- 客户端 chunks 先计大小/时间，再缓冲；结束时 global+route body hooks 各跑一次完整 body，发送插件返回的 bytes；无 buffers_body 则逐 chunk。`nico-http/src/proxy/hooks/body.rs:51-105,114-154`。**连接与请求头仍先送上游**，仅 body 被扣住，头去掉 Content-Length、改 chunked（`request.rs:395-397`）。E2E `plugins_buffered.rs:84-104,122-155` 对请求头先到、body 后到、单次 complete=true 均有直接断言。**[代码+测试]**
- README `:346-348` 的“上游要等最后一个字节才收到请求”若被理解为不会开连接/发请求头，则是失真；应区分 header 与 body，不据此认定实现 bug。内存不是预分配一份 max_body 的硬总 RSS 保证：客户端缓冲受入站 body bound，插件 API 会交/取 Vec，返回 body 也可变大（`plugin/chain.rs:290-319`、`plugin/context/contract.rs:158-165`）。**插件扩张后的字节与进程峰值内存未单独审计**。
- provider route 只允许插件 request/logging，禁 buffers_body 及声明 REQUEST_BODY/RESPONSE/RESPONSE_BODY 的使用链；global 不支持阶段会让含 provider route 的整份文档拒绝。`nico-server/src/llm/plan.rs:74-78,431-477`；`src/llm/tests/plan.rs:312-403` 覆盖接受 header、拒 body/response/buffer/global；runtime 仍跳过插件相应阶段（`nico-http/src/proxy/hooks/body.rs:67-75,172-180`、`response.rs:70-79`）。**[代码+测试]**

### 4.5 认证、rate_limit、全局身份的真实关系

- JWT/API key 在 request header stage 建立 typed identity，不写用户可伪造 header：JWT 验 HS256、exp/subject 等，API key 验 key 后用条目 name 作 subject。`nico-plugins/src/jwt.rs:156-185,211-251,279-292`、`api_key.rs:161-171`；`nico-server/tests/plugins_auth.rs:63-177` 覆盖可见 401/通过/日志归因。API key header 缺席默认 authorization Bearer；**显式配置任一 header**按原值匹配，包括显式 `header: authorization`，代码 `api_key.rs:110-115`；README `:391` 的简写可能未传达此差别，显式 authorization 场景未直接核对测试。
- rate_limit 每请求即时消耗一个 token，没有延迟排队；初始 burst 满桶，每秒补 per_second，桶数 max_keys=10000 默认、最多100000，LRU 淘汰。`rate_limit/limiter.rs:125-147`、`rate_limit/config.rs:16-20,75-105`。不足时拒绝并携 wait，默认 429 rate_limited + Retry-After；`nico-server/tests/plugins_limit.rs:73-149` 验先认证再限流、独立 subject 与 retry-after。**[代码+测试]**
- bucket key 是 hash `(route_id 或空串, instance_name, subject 或 peer_hash)`；`identity: subject` 默认，但无身份**回退 IP**，没 IP 再共用未知 bucket，不是“没认证就不限流”；`identity: ip` 忽略 subject。实现 `rate_limit.rs:103-125`；测试 `rate_limit/tests.rs:142-233` 覆盖 subject/route/instance 隔离与 fallback。同插件在多 route 用，不共享一份跨 route quota。**[代码+测试]**
- **全局不是永远没有身份。**global chain 开始时没 route id/identity；但先运行的 global auth 可 set_identity，后面的 global rate_limit 能读到。`plugin/context/contract.rs:196-229` 没有 scope 遮蔽，`request.rs:115-129` 没有在进入 route 时清 identity，global 自己也是 Authenticate 排在 Limit 前。故全局 default subject limiter 在 global auth 后可按 subject；没有 global auth、只有 route auth 时，全局 limiter 才先回退 IP。`config/resources.yaml:126-128` 的“全局只能按 IP”与实现调用链不符。`plugin/context/tests.rs:63-69` **只证明尚未认证的初始 context**，不能证明整条 global chain 都没有身份。**[代码推导；没有本轮直接全局 subject 分桶 E2E]**
- global 与 route 同用一个 rate_limit 实例会依次执行两次，但 global bucket route_id 为空、route bucket 有实际 id，是两个 key；全局与 route 限流叠加，不覆盖。上述 scope/键代码支持，未读到直接双使用点计费测试。
- 限流状态由同一个进程 factory 保留；同名、burst/per_second/max_keys 不变会复用 buckets，数值改变重建，非跨进程共享存储。实现 `rate_limit/factory.rs:45-57`、`limiter.rs:35-39`；测试 `rate_limit/tests.rs:306-399` 覆盖 reload、变参和失败发布不丢已发布表。**[代码+测试]**
- LLM token_budget 是另一种控制，按 `(route, 已认证 subject)`，没有身份会**跳过**（与 rate_limit 回退 IP 不同）；按事后 input+output 收费，下次请求超额才拒绝。`nico-http/src/quota.rs:164-204`；`quota/tests.rs:95-108,142-154` 验超额/route 隔离，未完整重验 auth→token meter 端到端。不要把请求令牌桶和 LLM token 预算当同一个限流概念。

## 5. README“所有响应不缓冲”专项核实

**结论：绝对表述不成立；插件接口与 convert front-end 是两个边界。**

| 路径 | 是否 whole-body 缓冲 | 证据 |
|---|---|---|
| 普通 upstream + 插件响应体 | 不提供全响应 buffers 开关；每 chunk 交插件 | `nico-http/src/plugin.rs:129-148`、`proxy/hooks/body.rs:198-245`；E2E `nico-server/tests/plugins_streaming.rs:81-133` |
| provider passthrough | 不建 transform，按 bytes 通过 | `proxy/hooks/llm.rs:223-224`、`body.rs:172-180`；`llm_passthrough.rs:85-147` |
| convert、2xx、客户端请求 stream=true | 逐事件转换，不 whole 累积，不受整包 response limit | `nico-llm/src/route/transform.rs:95-110,140-148`；`nico-llm/tests/route/transform.rs:75-105,261-273`；`nico-server/tests/llm_convert.rs:233-306` |
| convert、非流式成功或失败文档（包括请求 stream=true 却收到错误状态） | **缓冲**到结束后解码/编码；受 max_response_bytes | `nico-llm/src/route/transform.rs:112-135,146-187`；直接测试 `nico-llm/tests/route/transform.rs:54-71,163-191,225-257` |

- `README.md:347-348` 的“响应永远不可能被缓冲（可能是无限流）”与 `config/resources.yaml:104-107` 的类似绝对注释，若限定为**插件不能要求整响应**可成立，若概括网关所有响应则与上述代码/直接测试不符。
- `config/resources.yaml:10-15`、README 的 `max_response_bytes` 与 backend spec 本身已承认 buffered non-streaming response；同一文档存在范围不清，不必改 runtime 才能解释。
- “HTTP chunked”是传输 framing，不代表应用层响应一定 streaming：非流式 JSON 即便以 chunked 从 origin 到达仍会进 transform.buffer。`transform.rs:151-157`、`tests/route/transform.rs:241-257` 明确验证。
- 声明长度超限可在 head 写出前回 502；未知长度到 body 才超限时 head 已提交，只能断流，不能承诺总能回 502（`proxy/hooks/llm.rs:248-252`、`proxy/hooks/response.rs:277-286`）。本轮核对 `nico-http/src/proxy/tests/llm/response.rs:87-123,324-339` 和 transform 超限测试；未新增执行或穷尽所有 codec。

## 6. 哪些已是明确设计，哪些只是名称/文档误解风险

### 已明确且有直接测试，不应只因 nginx 名称相近就当 bug

1. host 比前缀等级高、byte prefix、重复组合拒绝：§2.1–2.2。
2. route timeout 更大时 runtime clamp；body/response 更大时 parse 拒绝；0 body=不接收 body：§2.4。
3. 逻辑 hostname 与连接 member 分离；Host 使用 member 实际 port；四种明确 picker；连续失败/半开 probe/全剔除拒绝：§3。
4. 插件定义与使用点分离；scope 内 auth→limit，scope 间 global→route；disabled 在全部使用点停编译/运行；buffer 只扣 body：§4。
5. LLM 的 protocol/mode 与普通代理不同；passthrough 只承诺 body 不解码而非 HTTP head 不动；convert 的非流式应答缓冲：§2.3、§3.2、§5。
6. 拒绝整个候选文档、保持上一已发布 generation：实现 `nico-server/src/reload/publish.rs:101-129,157-194,256-273`；插件真实服务测试 `nico-server/tests/plugins_config.rs:76-108`，LLM 发布测试 `nico-server/tests/llm_publication.rs` 的 `an_ai_route_is_published_with_its_snapshot_and_survives_a_refused_plan`。每请求从同一 publication 获取表与链，`nico-http/src/proxy/hooks/request.rs:73-95` 固定一次读取。**[代码+测试]**

### 已定位的误读/文档失真，供规划选范围，不作变更裁定

| 位置/名称 | nginx 用户或普通读者可能形成的错误预期 | 本轮事实 |
|---|---|---|
| “routes≈location” | 自动包含 server/listener 层级、路径规范化/前缀替换、父级策略继承 | 平面表；所有 listener 共用；exact host+raw byte prefix；只用命中的 route。§1–2 |
| README `:249` 文档顺序 tie-break | 相同匹配可后写覆盖或靠排序解冲突 | 重复 match 在 publication 前已拒绝。§2.2 |
| 示例 30000/120000；README `:239` | route 放宽到两分钟，或相反更大数一定报错 | 数值允许写入，但实际全局 30s。§2.4 |
| README `:347-348`、config `:104-107` 响应永不缓冲 | convert 非流式也不缓存整包 | 仅插件边界不支持 whole response；convert 有显式 bounded buffer。§5 |
| README `:346` buffered 后上游才收到“请求” | 完整上传前不开连接、不发头 | 请求头先到、body 后到，有真实测试。§4.4 |
| config `:126-128` 全局“没有 identity、只能 IP” | 不能全局认证后按 subject 限流 | global auth 建身份后后续可读取；route_id 仍为空。§4.5 |
| upstream `host/port` 与 endpoint `address/port` 的注释 | endpoint port 只影响 socket，Host 完全固定 | Host hostname 固定、非默认 port 跟选中 member。§3.2 |
| `least_conn` / `ip_hash` / `weight` / `max_fails` / `fail_timeout_ms` | 相同名字足以证明与 nginx 同算法/加权/时间窗/failover | 请求数而非 pool socket、rendezvous IP/name、仅 weighted 策略读 weight、连续失败后冷却、traffic 本次失败不换 member。§3 |
| 插件声明在根部 / 同名多处引用 | 顶层定义就全局启用；route 覆盖 global；同名只跑一次 | 使用点显式；两 scope 叠加且同实例可跑两次。§4 |
| `identity: subject` | 无认证就无限额，或全 route 同用户共一桶 | rate_limit 回退 IP，route id 是桶键一部分。token_budget 才在无身份时跳过。§4.5 |
| `protocol` / `mode: passthrough` | HTTP transport 控制或 proxy_pass 原样 HTTP | LLM body 方言与转换选择；provider head 仍被编译并改写。§2.3、§3.2 |

**另一个可跟踪但未扩展审计的字段消费问题**：README `:424` 展示 `providers[].timeout_ms`。模型保存/校验该值（`nico-core/src/config/provider.rs:151-155`、`semantic.rs:165-166`），但本轮完整读取的 provider facts/convert member/passthrough 编译链没有传递它（`nico-server/src/llm/plan/facts.rs:37-73`、`plan/chain.rs:179-246`、`plan.rs:340-377`），peer 时间取 HttpContext request/route budget（`nico-http/src/proxy/hooks/request.rs:201-218`）。仓库调用搜索只见其 accessor 的校验/模型测试，**未找到 runtime consumer 或“provider timeout 生效”的测试**。这比单纯名称歧义更需要后续确认，但本轮不决定移除/补消费，不将其当成已复现产品 bug。

## 7. 未验证/不应从本报告推出的结论

- 研究子代理未运行 cargo test、未做全量审计；本文既有测试名称与源码定位属于静态证据。主会话执行过的测试结果单列于 `verification.md`。
- 未重验所有 HTTP request-target（编码斜杠、非 UTF-8、absolute/authority/asterisk）、DNS 多地址/变化、不同 listener 路由矩阵；只保证所引用 matcher/consumer 的明确行为。
- endpoint validation 的一些负例只有代码证据；不声称重复地址经 DNS 解析去重或所有 host 特殊字符串都被拒绝。
- global auth→subject limit、同名实例跨 scope 两次执行/不同 bucket，为代码调用链推导，未读到专门 E2E；初始空 identity 单测不替代此场景。
- 未验证已有 ejection 热改 max_fails=0、所有并发健康结果归属、每种 I/O stall 恰在绝对 deadline 触发；不扩大为可靠性漏洞结论。
- 不评判所有 provider codec、token 预算的全部 auth/计费链、流式内部 accumulator；只取与资源 schema 边界直接相关的 front-end/transform。
- 未引入 nginx 官方等价判断、未设计 server/location 新 schema、未修改 README/spec/示例/产品代码，未提交 commit。
