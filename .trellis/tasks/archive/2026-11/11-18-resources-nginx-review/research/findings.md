# resources.yaml 与 nginx 资源配置对齐审查

建议保持 `routes`、`upstreams`、`endpoints`、`plugins`、`global_plugins` 的平面结构和现有名称，先修正文档与示例，再按部署场景决定是否补能力。
这些对象的职责已经清楚；加一层 `server/location` 或照搬 nginx 策略名，不能自动获得监听隔离、URI 替换、共享限额或算法兼容。
现行健康模型与路由预算规则有明确代码和测试支持，应保留；候选重命名只供后续评估，本报告不批准任何 schema 或行为变更。

## 1. 证据与判断边界

- 审查日期：2026-11-18；nginx 官方资料访问日期同日，来自本任务既有取证，页面为滚动文档，不承诺某个完整 nginx 版本兼容。
- 内部锚点均为仓库根相对 `file:line`；“代码推导”表示消费路径明确，“现有测试”表示已读取断言，不自动表示本轮运行了对应集成测试。
- 两侧依据分别为 `.trellis/tasks/11-18-resources-nginx-review/research/current-resource-contract.md:1` 与 `.trellis/tasks/11-18-resources-nginx-review/research/nginx-resource-semantics.md:1`；下文保留决策所需原始锚点及官方 URL，可独立阅读。
- 主代理在规划阶段已实际执行 core/http/lb/plugins 库测试，共 829 项通过；本报告阶段仅定点回读源码和检查文档，不重复运行测试。
- 未启动 nginx、未执行 nginx→nico 实进程差分；下述反例是代码/现有测试与官方语义的比较，不能写成每个 wire 差异均已实测。
- “条件映射”只指所列前提下的目标或局部行为可对应；“无直接对应”指当前公开配置不能表达，不否认内部 trait 的扩展空间。

## 2. 有条件的对应表

| nginx 概念 | nico 现行契约与等价条件 | 证据与官方来源 |
|---|---|---|
| `listen → server_name → location` | **条件映射**：仅需所有入口共享一张表、exact host 加前缀时可表达业务分流。所有 listeners 共用路由表；不是先固定虚拟主机再选路径，限定 host 无路径命中可落无 host 路由。 | `crates/nico-http/src/service.rs:98`；`crates/nico-http/src/route.rs:63`；https://nginx.org/en/docs/http/request_processing.html |
| 前缀 `location` 与冲突 | **条件映射**：nico 在真正命中 path 的候选内先比 host 等级再比前缀长度；ASCII host 忽略大小写/入站端口，不支持 server_name 通配/正则。重复 `(host_key,path_prefix)` 发布前拒绝；列表排序不是冲突解决。 | `crates/nico-core/src/config/host.rs:72`；`crates/nico-core/src/config/semantic.rs:232`；`crates/nico-http/src/route/tests.rs:144`；https://nginx.org/en/docs/http/ngx_http_core_module.html#location |
| URI 规范化、精确/正则路径、尾斜杠 | **无直接对应**：nico matcher 对 `uri.path()` 做 byte prefix，query 不参与；自身不 decode、消 dot 或合并斜杠，无 `=`/正则/自动补斜杠配置。特殊 URI 经 Pingora 的完整矩阵未跑。 | `crates/nico-http/src/route.rs:65`；`crates/nico-http/src/route/tests.rs:40`；`crates/nico-core/src/config/semantic.rs:320`；https://nginx.org/en/docs/http/ngx_http_core_module.html#location |
| `proxy_pass` 不带 URI / 带 URI | **条件映射 / 无直接对应**：普通 upstream 保留路径，不自动剥离 route prefix；只在无需 rewrite 的普通目标上近似不带 URI 的代理。带 URI 的 location 前缀替换没有公开字段。 | `crates/nico-core/src/config/upstream.rs:15`；`crates/nico-http/src/proxy/hooks/request.rs:328`；`crates/nico-http/src/proxy/hooks/llm.rs:50`；https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_pass |
| upstream 名称、连接地址、Host/SNI | **条件映射**：`name` 是引用键，`host` 是逻辑 hostname/SNI，endpoint address/port 决定连接；Host 包含选中 endpoint 的非默认端口。没有任意 `proxy_set_header` 公开配置；DNS 请求时只取首个结果，不展开为多个成员。 | `crates/nico-http/src/upstream/pools.rs:292`；`crates/nico-http/src/upstream.rs:147`；`crates/nico-http/src/upstream.rs:223`；https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_set_header ；https://nginx.org/en/docs/http/ngx_http_upstream_module.html#server |
| 默认轮询与 `weight` | **条件映射**：nico `round_robin` 不消费 weight，只有 `weighted_round_robin` 使用 smooth weighting；nginx 默认加权轮询。权重全 1 时才可忽略这一差异，不承诺精确选择序列兼容。 | `crates/nico-lb/src/picker.rs:164`；`crates/nico-lb/src/picker.rs:204`；`crates/nico-http/src/upstream/pools/tests.rs:173`；https://nginx.org/en/docs/http/ngx_http_upstream_module.html#server |
| `least_conn` | **条件映射**：nico 比在途请求数、平票轮转、不加权；nginx 比活跃连接且考虑权重。请求/连接复用关系及权重会影响结果，不能把同名当作同算法。 | `crates/nico-lb/src/picker.rs:284`；`crates/nico-http/src/upstream/pools/member.rs:154`；`crates/nico-http/src/upstream/pools/tests.rs:203`；https://nginx.org/en/docs/http/ngx_http_upstream_module.html#least_conn |
| `ip_hash` / sticky | **条件映射**：nico 用完整 socket peer IP bytes 与 endpoint name 做 FNV-1a rendezvous；无 peer 时轮询。nginx IPv4 取前三段、IPv6 取完整地址；不能承诺原 sticky 映射或 ketama 兼容。 | `crates/nico-lb/src/weighted.rs:32`；`crates/nico-lb/src/weighted.rs:148`；`crates/nico-http/src/upstream/pools/tests.rs:238`；https://nginx.org/en/docs/http/ngx_http_upstream_module.html#ip_hash |
| `max_fails` / `fail_timeout` | **条件映射**：nico 是连续失败阈值加剔除冷却，默认 3/30000ms；nginx 官方描述要求失败发生于 fail_timeout 期间，该值也作停用时长，默认 1/10s，且有单成员忽略规则。不能据此推断任意成功均不清零或实现为严格滑动窗口；版本限定的计数反例见 C5。nico 无该单成员豁免。 | `crates/nico-core/src/config/upstream.rs:32`；`crates/nico-http/src/upstream/pools/health.rs:112`；`crates/nico-http/src/upstream/pools/tests/health.rs:77`；https://nginx.org/en/docs/http/ngx_http_upstream_module.html#server |
| 被动失败分类 / `proxy_next_upstream` | **无直接对应本次重试能力**：nico traffic 连接/DNS/握手类失败及 origin 5xx 计失败，非 5xx 应答清零；不自动给当前请求换 endpoint，全不可用回 502。nginx 默认 error/timeout，可配置部分状态，重试受提交/幂等边界约束。 | `crates/nico-http/src/proxy/hooks/response.rs:223`；`crates/nico-http/src/context/attempt.rs:57`；`crates/nico-server/tests/upstream_lb.rs:184`；https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_next_upstream |
| 插件声明、使用点、执行阶段 | **条件映射**：`plugins` 只定义实例，global/route 列表才引用；scope 内 Authenticate→Limit→Other，同阶段保留使用点顺序；先 global 后 route，响应 scope 反向。不是 nginx 文本指令顺序，nginx 模块在 PREACCESS/ACCESS 等阶段运行。 | `crates/nico-plugins/src/compile.rs:450`；`crates/nico-http/src/proxy/hooks/request.rs:109`；`crates/nico-http/src/proxy/hooks/response.rs:88`；https://nginx.org/en/docs/dev/development_guide.html#http_phases |
| `auth_basic` / `auth_request` | **无直接对应**：现有 jwt/api_key 是网关本地 JWT/Bearer 或原值 key 核验，非 Basic 密码文件/外部子请求；多个认证必须都过，第一身份保留。disabled 影响全部使用点，不能当 route 局部关闭。 | `crates/nico-plugins/src/jwt.rs:156`；`crates/nico-plugins/src/api_key.rs:110`；`crates/nico-http/src/plugin/chain.rs:265`；`crates/nico-plugins/src/compile.rs:213`；https://nginx.org/en/docs/http/ngx_http_auth_basic_module.html ；https://nginx.org/en/docs/http/ngx_http_auth_request_module.html |
| `limit_req` / 同 zone 共享 | **条件映射**：nico 为即时 token bucket，初始 burst 满、不排队，默认 429；nginx 漏桶默认延迟、默认 503，可设 nodelay/status。nico key 含 route/实例/subject 或 peer，同实例多 route 分桶；global route_id 为空。 | `crates/nico-plugins/src/rate_limit/limiter.rs:125`；`crates/nico-plugins/src/rate_limit.rs:103`；`crates/nico-plugins/src/rate_limit/tests.rs:142`；https://nginx.org/en/docs/http/ngx_http_limit_req_module.html |
| `limit_conn` | **无直接对应**：每请求消耗速率 token 不限制同时在途请求；HTTP/2/3 并发也不能用 per_second 替代。 | `crates/nico-plugins/src/rate_limit/limiter.rs:125`；`crates/nico-plugins/src/lib.rs:64`；https://nginx.org/en/docs/http/ngx_http_limit_conn_module.html |
| realip 与 IP 策略 | **无直接对应可信地址替换**：nico IP hash/限流读 socket peer，不信任 XFF 为原客户端；现有转发头处理不构成 realip 信任配置。 | `crates/nico-http/src/proxy/forward.rs:101`；`crates/nico-http/src/proxy/hooks/request.rs:69`；https://nginx.org/en/docs/http/ngx_http_realip_module.html |
| `client_max_body_size` / response ceiling | **条件映射**：nico request body=0 仅允许空体，nginx 0 不限；route body/response 超文档上限拒绝发布。response bound 仅 convert 整包响应，不是 traffic 输出流量上限；nginx body 指令不提供该响应语义。 | `crates/nico-core/src/config/request.rs:88`；`crates/nico-core/src/config/semantic/tests/numeric.rs:81`；`crates/nico-http/src/proxy/hooks/llm.rs:223`；https://nginx.org/en/docs/http/ngx_http_core_module.html#client_max_body_size |
| 总预算与 I/O 间隔 timeout | **条件映射**：nico document→route timeout 取 min，沿同一起点收紧；route 写大仍合法。`ttft_timeout_ms` 实际约束每次 upstream read、含 body/chunk 间隔；不能把总 deadline 当 nginx client_body/proxy_read timeout。 | `crates/nico-http/src/budget.rs:80`；`crates/nico-http/src/context.rs:115`；`crates/nico-http/src/proxy/hooks/request.rs:239`；`crates/nico-http/src/upstream.rs:201`；https://nginx.org/en/docs/http/ngx_http_core_module.html#client_body_timeout ；https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_read_timeout |
| 请求与响应 buffering | **条件映射**：插件 buffers_body 只扣请求 body，连接/头先发；普通响应逐 chunk。convert 非流式（含错误文档）有 bounded whole buffer，成功事件流逐事件转换；没有 nginx 通用响应 buffer/临时文件配置。 | `crates/nico-http/src/proxy/hooks/request.rs:380`；`crates/nico-server/tests/plugins_buffered.rs:84`；`crates/nico-llm/src/route/transform.rs:95`；https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_request_buffering ；https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_buffering |
| provider / protocol / token_budget | **无直接对应**：protocol 是 LLM 客户端方言，convert/passthrough 是 body 处理方式，provider path/header 可改写；token_budget 是事后 input+output 计费、无身份跳过，不是 limit_req 请求 token。nginx upstream/limit_req 只能作对照边界。 | `crates/nico-core/src/config/reference.rs:324`；`crates/nico-http/src/proxy/hooks/llm.rs:50`；`crates/nico-http/src/quota.rs:164`；https://nginx.org/en/docs/http/ngx_http_upstream_module.html ；https://nginx.org/en/docs/http/ngx_http_limit_req_module.html |

## 3. 能区分行为的具体反例

以下均是迁移验证用例，不是本轮实进程差分结果；每例注明已有证据与尚需执行的验收。

### C1：host 已出现不代表进入独立 server

设路由 A 为 `host: api.example, path_prefix: /admin/`，B 无 host、前缀 `/public/`。
请求 `Host: api.example`、`GET /public/x` 在 nico 仍可命中 B；若 nginx 已选定 api.example 的 server，其他 server 的 `/public/` 不会参与补位。
另设限定 host 的 `/` 与无 host 的 `/api/v1/`，同 host 请求 `/api/v1/x` 会选限定 host `/`，不是跨全表单纯最长前缀。
证据：`crates/nico-http/src/route.rs:63`、`crates/nico-http/src/route/tests.rs:64`；前一负例是循环推导，后一优先级有现有测试。
验收：origin 记录路由目标；经两个 listeners 重复请求，另加同 `(host_key,prefix)` 文档，确认候选被拒且旧 generation 继续服务。

### C2：byte prefix 与 nginx location 细节

配置 `/api` 会接受 `/apix`，配置 `/api/` 不接受 `/api`，nico 无对应自动 301；query 不参与选择。
nginx `location /api/` 加代理时，请求恰少末尾斜杠可触发补斜杠 301；规范化规则也不同于 matcher 自身。
证据：`crates/nico-http/src/route/tests.rs:40`；https://nginx.org/en/docs/http/ngx_http_core_module.html#location 。
验收：对 `/api`、`/api/`、`/apix?q=1` 记录状态/目标；编码斜杠、点段、连续斜杠须另跑 Pingora 全路径矩阵后再写 wire 结论。

### C3：同一 `/api/` 规则不自动替换转发路径

nginx `location /api/ { proxy_pass http://backend/internal/; }` 把 `/api/x` 发成 `/internal/x`。
nico `/api/ → upstream` 的普通路径无替换，预期仍是 `/api/x`；不能在 upstream 名称或 host 后拼 `/internal/` 模拟 URI 字段。
证据：`crates/nico-core/src/config/upstream.rs:15`、`crates/nico-http/src/proxy/hooks/request.rs:328`；https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_pass 。
此例是源码推导，未找到专门 `/prefix/x?query` 原样到 origin 的直接 E2E；验收须记录 origin request-target（含 query），不只断言状态 200。

### C4：weight 写了不等于生效；IP sticky 也不是同一映射

两个可用 endpoint 权重为 3:1，nico 默认 `round_robin` 持续轮转，不产生按该权重的 3:1；选择 `weighted_round_robin` 才消费权重。
`least_conn` 即使权重不等也只看在途请求；两个同 /24 的 IPv4 来源在 nginx ip_hash 使用相同键，在 nico 使用不同完整 IP 键，可能分到不同成员。
证据：`crates/nico-http/src/upstream/pools/tests.rs:173`、`crates/nico-http/src/upstream/pools/tests.rs:186`、`crates/nico-lb/src/weighted.rs:148`。
验收：固定可用集合测选择分布，长请求测 least_conn，固定 IP/endpoint name 对照映射；不预设不同 IP 必定落不同成员。

### C5：同名健康参数会产生不同剔除与失败可见性

显式阈值 3、时长 30s。限定 nginx **1.28.0 默认 round-robin、至少两个成员、单 worker、无并发/重载**，对同一成员在 t=0/1/2/3s 顺序完成 `连接失败、200 成功、连接失败、连接失败`；这些时间均在首次失败后的 30s 内，失败使用默认会计数的连接错误。nico 在成功时清零，最后连续仅两次，不剔除。
nginx 此版本选择该成员时，只有 `now - checked > fail_timeout` 才更新 checked；失败时累加 fails 并同时更新 accessed/checked；成功只在 `accessed < checked` 时清零。因此上述 t=1 成功不清零，第三次失败后该成员暂不可选。若成功发生于超过 fail_timeout 后的一次新选择，checked 已前移，成功可以清零；不能说 nginx 的成功一概不清零，也不能将官方时间窗措辞当作严格滑动窗口实现。
上述 nginx 结论是**官方发行源码的静态推导，尚未实进程复现**。来源：https://nginx.org/download/nginx-1.28.0.tar.gz ，包内 `src/http/ngx_http_upstream_round_robin.c:736-740,774-776,819-848`（选择排除、checked 前移、失败累加与成功清零），访问日期 2026-11-18。其他版本、picker、并发情形不由此例保证；部署验收须保证事件确实属于同一成员，且无隐藏重试增加计数。
更独立的差异是默认值分别为 nico 3/30s、nginx 1/10s；nico 即使只有一个成员也可剔除，全部不可用回 502；撞死成员的那次 traffic 请求不自动改投健康成员。
内部证据：`crates/nico-http/src/upstream/pools/health.rs:136`、`crates/nico-server/tests/upstream_lb.rs:184`；官方参数/单成员语义：https://nginx.org/en/docs/http/ngx_http_upstream_module.html#server 。
验收：记录目标成员与后续是否获选，对照窗内成功/跨窗后成功两组序列，另测单成员、全剔除、冷却 probe；分别记录当前请求状态与后续选择，不能只测最终恢复。

### C6：同实例跨 route 不是一份共享 quota

`/a`、`/b` 都引用同一限流实例，burst=1；同 subject 在无补充时间内各发一次，两 route 分别有一桶，可各接受一次。
nginx 两 location 使用相同 zone 且 key 相同，共享状态；迁移为两次 route 引用会放宽跨路径总额度。
将实例放 global 可得到 route_id 空的 bucket；global auth 在同 scope 的 limiter 前建 subject，可按 subject 分桶；只有 route auth 时 global limiter 先回退 peer IP。
证据：`crates/nico-plugins/src/rate_limit/tests.rs:142`、`crates/nico-plugins/src/rate_limit.rs:103`、`crates/nico-http/src/proxy/hooks/request.rs:109`。
跨 route 隔离有现有测试；global auth→subject 及同实例 global+route 两次执行是调用链推导，未找到专门 E2E。
验收：冻结补充时间，测两个 subject、两个 route、global/route 双引用及无身份回退；不要靠紧邻真实时钟请求保证确定性。

### C7：0 的含义相反，route timeout 写大也不会放宽

将 nginx `client_max_body_size 0` 翻译成 nico `request.max_body_bytes: 0` 后，1 byte body 会被拒；nginx 的 0 则关闭该检查。
示例 `request.timeout_ms: 30000` 与 route `timeout_ms: 120000` 可解析，但运行预算仍 30s；route body/response 写得超过文档则直接拒绝。
证据：`crates/nico-http/src/context/tests.rs:222`、`crates/nico-http/src/budget/tests.rs:109`、`crates/nico-core/src/config/semantic/tests/numeric.rs:81`；示例 `config/resources.yaml:17`、`config/resources.yaml:175`。
验收：空/非空 body、route 数值高低的解析与 runtime 两套断言；慢上传/长响应还需检查提交前状态码与提交后断流，不能假设总能返回新错误页。

### C8：首 token 到了也不代表该 timeout 结束

LLM route 设置 `ttft_timeout_ms: 1000`，首响应很快、后续一次读取等待超过 1s，仍受该 read timeout 约束，非流式读取亦然。
这不是“只等首 token”；也不能以持续小 chunk 保证超出 request 总预算的无限长流被允许。
证据：`crates/nico-http/src/proxy/hooks/request.rs:239`、`crates/nico-http/src/upstream.rs:201`、`crates/nico-http/src/upstream/tests.rs:110`。
验收：首头延迟、两 chunk 间隙、非流式慢读、总预算耗尽分别测试；现有预算单测不替代所有实际 I/O stall 的定时保证。

### C9：缓冲与 passthrough 的名称不能概括整个 HTTP 行为

上传插件启用 buffers_body 时，origin 可先收到请求头、后收到完整 body；不是最后一个字节到达前完全没有 upstream 请求。
convert 非流式 JSON 即使用 chunked 传输也整体缓冲，超过 response bound 被拒/断流；成功事件流不整体缓存。
provider passthrough 保持 body 不解码，但会采用 provider path/headers；期待 HTTP target/Authorization 原样的迁移会失配。
证据：`crates/nico-server/tests/plugins_buffered.rs:84`、`crates/nico-llm/tests/route/transform.rs:241`、`crates/nico-server/tests/llm_passthrough.rs:116`（均为现有测试静态证据）。
验收：origin 分别记录头抵达时点、body、path、凭证头；区分非流式成功、错误文档、事件流与超限时是否已提交 head。

## 4. 后续建议的共同约束

当前交付仅报告，现有 YAML、traffic/llm 构建、请求行为及 reload/LKG 均无变化。以下每项是可选择的后续边界。
任何 schema 候选都应覆盖旧 YAML、错误信息、模型/编译/消费者、测试、README/示例/spec 与 LKG 缓存文档；不能只改显示名称。
严格解析拒绝未知字段，若引入别名需定义新旧并存冲突和弃用期；无效候选整份拒绝，已发布表与在途 generation 保持一致。
发布锚点：`crates/nico-server/src/reload/publish.rs:101`、`crates/nico-server/src/reload/publish.rs:157`；请求固定快照：`crates/nico-http/src/proxy/hooks/request.rs:73`。
traffic-only 可声明未引用 provider，但 provider route 拒绝发布：`crates/nico-server/src/reload/llm.rs:137`；后续不得为对齐改变这一能力边界。

## 5. 文档问题：按误配影响优先、同等影响下按修正代价排序

### D1｜高影响、低代价：总预算与数值 0

场景：运维照示例部署两分钟 LLM，实际 30s；或将 nginx 的不限 body 写成 0，导致所有非空上传失败。
建议：修正 `config/resources.yaml:17` / `config/resources.yaml:175` 的组合示例；若展示两分钟需文档总预算也允许，或明确 route 值被截断；`README.md:239` 区分 parse 接受、runtime 生效。
同时在 body 说明显著列出“0=仅空体”，在 body/response/timeout 表中逐项区分拒绝与 clamp，不统一称为继承。
影响：纯说明对 traffic/llm 和 reload/LKG 无行为代价；示例未来若改总预算须说明会放宽其他 route 的默认预算，不作为无害措辞改动混入。
验收：复用 C7，自动校验示例预期有效预算；延时场景另外执行集成测试，不把 parser 成功当两分钟生效。

### D2｜高影响、低代价：全局身份与限流共享范围

场景：想按用户跨 API 总限流，却把同一实例分别挂两个 route；或看到“全局只能 IP”而放弃 global auth 后限流。
建议：改 `config/resources.yaml:126` 的绝对表述；补 bucket key、subject 缺席回退 IP、global/route 叠加、global→route 与 scope 内 auth→limit 的时序图解。
影响：文档修正不改变 traffic/llm 或 LKG；运维若随后移动使用点会改变计费/拒绝范围，应作为策略发布单独审阅，不能称语义无损迁移。
验收：C6 的 global subject E2E 和双引用用例补齐后再升级为运行保证；注明进程内状态，不承诺跨副本共享。

### D3｜高影响、低代价：权重、健康、当前请求重试

场景：迁移 3:1 upstream、单成员服务或期待 5xx 自动换成员；仅复制同名字段会改变分配和故障暴露。
建议：在 `README.md:254` 上游说明邻近位置附策略矩阵；标默认 RR 不读 weight、least_conn 请求计数、IP/name 键、默认 3/30s 与连续失败、traffic 无同请求 failover。
影响：保留现有模型，traffic 无行为变更；LLM attempt/pool 另列，不把其 retry/fallback 承诺扩展给 traffic；LKG 无迁移成本。
验收：C4/C5；健康状态 reload 延续另测同身份、改地址、调阈值，避免仅由文案暗示切换模型。

### D4｜高影响、低代价：路由边界、URI 与重复条件

场景：把一个 listener 当一个 vhost，或认为 host 命中后绝不回退，或靠 route 列表顺序覆盖同一规则。
建议：修正 `README.md:249` 的可操作性暗示，明确合法文档重复规则不可达；补所有 listener 共享、host/path 同轮选候选、byte prefix、不剥离前缀且不继承未选中 route 策略。
影响：traffic/llm 同用该 matcher；只改说明不影响 reload/LKG，运维删除/合并重复规则仍需完整候选验证，不提供按顺序覆盖的迁移脚本。
验收：C1—C3；特殊 URI 结果待矩阵完成再增补，不先宣称每个编码输入都已观察到不同。

### D5｜中高影响、低代价：缓冲与 provider passthrough

场景：上传服务依赖“全 body 到齐前绝不建上游请求”，或为省内存假设 LLM 所有响应均不缓存。
建议：修正 `README.md:346`、`config/resources.yaml:104` 的绝对措辞，区分请求头/body、插件响应接口/convert buffer；passthrough 文案列出 provider path/header 改写。
影响：traffic 与 llm 分开说明；没有 schema/reload/LKG 变化。内存预算不能只写 body ceiling×并发就保证 RSS，插件返回体扩张/峰值尚未专项审计。
验收：C9，并记录 head 已提交时超限只能断流；不以 HTTP chunked 代替应用层 streaming 分类。

### D6｜中影响、低代价：authority 端口与插件启用

场景：endpoint 改端口后 origin Host 校验失败；停一个实例误以为只停当前 route，或以为根部声明即全局运行。
建议：澄清 `crates/nico-core/src/config/upstream.rs:141` 注释中的 hostname/完整 authority；插件文档列定义/使用点、enabled 全实例生效、所有 enabled 定义都会编译。
影响：仅说明，无 traffic/llm 或 LKG 变化；未来真要 route override 必须单独设计，provider route 的 body/response 插件禁用阶段保持明确。
验收：`crates/nico-http/src/upstream/tests.rs:192`、`crates/nico-plugins/src/compile/tests/plan.rs:59`；加 unused enabled 错配置的负例才可宣称该场景直接测试覆盖。

## 6. 命名决策与待确认消费路径

### N1｜保留资源名和现有策略入口；暂不重命名健康字段

场景：nginx 使用者需要识别服务、成员、策略实例；现有名称已能导航，套 server/location 会增加未经实现的继承暗示。
保留 routes/upstreams/endpoints/plugins/global_plugins；`round_robin`、`weighted_round_robin` 通过行为矩阵区分；`least_conn`/`ip_hash` 保留但明确算法边界。
健康继续采用现有连续失败与冷却模型。可评估 `fail_streak_threshold` / `ejection_cooldown_ms` 是否比旧名更少误解，当前不采用，也不改变 3/30s 默认。
影响：保持现状对 traffic/llm 与 LKG 零迁移；候选若采纳须处理严格 YAML 兼容与缓存、新旧冲突，明确是 traffic upstream 健康而非自动重命名 provider breaker。
健康状态按 upstream/endpoint name/address/port 继承，参数与 generation 分开：`crates/nico-http/src/upstream/pools.rs:75`、`crates/nico-http/src/upstream/pools.rs:189`；别名不能意外重置状态。
验收：用 C4/C5 证明用户理解差异，若重命名再验旧文档读取、别名冲突、失败发布保留状态及在途 generation；收益不足即保留原名。

### N2｜候选 `read_timeout_ms`：先判断值不值得迁移

场景：用户想限制首 token，结果后续长 chunk 间隙也超时；当前 `ttft_timeout_ms` 暗示范围比实际窄。
建议先统一注释与说明；可评估改称 `read_timeout_ms`，其语义仍是每次 upstream read 与 remaining budget 取小，不暗示提供真实首 token timer。
影响：主要作用于 LLM route；traffic 当前拒绝该字段，不应因改名顺便扩展。严格 schema/旧 YAML/LKG 需要别名或版本迁移，编译 plan/错误/spec/测试一并调整。
验收：C8 加新旧字段互斥与旧配置恢复；若真正需要 first-token-only，则另作能力设计，不能只改名却改变既有流式超时行为。

### N3｜高优先待确认：`providers[].timeout_ms` 的 runtime 消费

场景：用户照 `README.md:424` 设置 provider timeout，期待它独立收紧请求，却未有明确生效证据。
事实：模型保存并校验，见 `crates/nico-core/src/config/provider.rs:151`、`crates/nico-core/src/config/semantic.rs:165`；已读 facts/attempt/passthrough 链未传递该值。
核查入口：`crates/nico-server/src/llm/plan/facts.rs:37`、`crates/nico-server/src/llm/plan/chain.rs:179`、`crates/nico-server/src/llm/plan.rs:340`；peer 当前读 context budget：`crates/nico-http/src/proxy/hooks/request.rs:201`。
本次定点搜索未找到 runtime consumer/生效测试，结论为待确认消费路径，不能定为已复现 bug；当前不补消费、不删字段，也不改默认。
影响：后续需分别核实单 provider、pool、fallback、passthrough；traffic-only 未引用 provider 的处理也须保留。若补消费/删除会改变旧配置语义或接受范围，涉及 LKG 兼容，不能当文档修复。
验收：给 provider/route/document 不同预算，跑慢头/慢体实验定位哪一层生效；先形成最小复现，再决定说明、补消费还是弃用及原子发布测试。

## 7. 独立能力缺口：只按真实场景选后续任务

### G1｜高影响、中高代价：URI 替换与更丰富路由

场景：现有 `/api/ → /internal/` origin，或不同 listener 同 host/path 必须隔离；当前公开配置不能直接表达。
建议：先选最小需要的 rewrite 或入口隔离能力，精确/正则/虚拟主机另有需求再评估，不为 nginx 外观一次引入层级树。
影响：traffic 与 llm matcher 都可能受影响；provider path 仍由适配器所有，不能把 rewrite 静默用于转换链；新字段需旧 YAML/LKG 策略与冲突规则，失败整份拒绝。
验收：C1—C3、query/编码/尾斜杠矩阵、match 前后改写顺序、重复条件、新旧 generation；不能只测试单条正常路径。

### G2｜高影响、高代价：普通 traffic 的同请求 failover

场景：无状态 GET 希望死成员时本次即可走健康成员；当前被动剔除只改善后续请求。
建议：独立定义失败分类、幂等性、body 可重放、次数/时间预算、响应提交后禁止重试；保留现有健康模型，不借此默认迁移成窗口累计。
影响：traffic 新能力不得套用 LLM attempt 的分类；llm retry/fallback 继续受原协议约束；reload 下每请求固定候选/预算，LKG 和健康继承不得部分发布。
验收：连接失败、已发非幂等 body、超过 replay 能力、响应 head 已提交、全坏与冷却恢复；确认重试不会放大总预算或双写业务请求。

### G3｜中高影响、中高代价：共享限额、并发与可信客户端地址

场景：跨 route 同用户一份 quota、多副本总额度、长请求并发保护、前置代理后按真实用户 IP 选择；这些是不同需求。
建议分别选择 route-independent key/显式共享范围、分布式状态、并发 limiter 或可信代理模型；不用现有 token bucket 同时承诺这些能力。
影响：traffic/llm 都需定义身份与计数时点；LLM token_budget 保留专用计费含义。状态 key/生命周期改变须定义 reload 是否继承、拒绝候选不得清空已发布桶，LKG 新字段需迁移。
现有复用规则按同名及 burst/per_second/max_keys 判断：`crates/nico-plugins/src/rate_limit/factory.rs:45`；不是跨进程共享存储。
验收：C6、HTTP/2 并发、取消释放、reload/失败发布状态、多副本一致性；可信代理需单测伪造 XFF 不越过信任边界。

### G4｜中影响、中高代价：通用 HTTP 缓冲与 header 控制

场景：慢客户端需响应缓冲解耦、origin 要求固定 Host/header，或上传需完整读取后才连上游；现有插件 body 缓冲不等于这些开关。
建议：按具体方向设计有界内存/磁盘/时间及凭证头归属，不把 provider passthrough 扩成通用原样代理，也不将内部扩展接口称为已部署能力。
影响：traffic 先定义公开控制；llm transform 和 credential 所有权须独立保护；新 schema、临时资源清理、reload 与在途旧 generation 行为均需明确，失败不影响 LKG。
验收：C9、慢上传/下载、取消清理、磁盘/内存上限、头覆盖权限；若无真实部署需求，保留当前能力并完善说明即可。

## 8. 验证结果与交付范围

实际已执行：`cargo test -p nico-core --lib --locked`，236 passed；`cargo test -p nico-http -p nico-lb -p nico-plugins --lib --locked`，593 passed；合计 829。
执行记录：`.trellis/tasks/11-18-resources-nginx-review/research/verification.md:1`；这些是库测试绿色结果，报告里的 server/LLM 集成测试引用是静态断言证据。
本报告阶段回读了 router/校验/预算、picker/健康、插件排序/分桶、头改写/transform、provider 编译与 publication 关键路径，仅新增本文件。
本阶段文档检查：两份 JSONL 清单 validate 通过（超注入大小的两份文件已手动完整读取）；108 处仓库 file:line 均存在且行号有效，19 行对应表均附原始源码锚点与官方 URL；无尾空白且有结尾换行。
未运行 server E2E、nginx 差分、全量 Rust fmt/clippy/doc/feature matrix；无产品代码变更，不以这些未运行检查冒充覆盖。
尚未解决的证据问题是 provider timeout 消费路径、global auth→subject 及跨 scope 双计费专门 E2E、特殊 URI/多 listener 完整矩阵。
DNS 动态/多地址、已剔除后热改 max_fails=0、并发健康结果、所有 I/O stall 的 deadline 即时性与插件扩张峰值内存均未扩展审计。
这些边界不阻止本次报告交付；后续选定事项按上述验收补证，不将已明确差异重新归类为新 bug，也不要求当前审查先实现 nginx 全功能兼容。
