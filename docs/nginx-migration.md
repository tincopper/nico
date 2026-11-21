# 从 nginx 迁移到 nico

本文对应 `apiVersion: gateway/v1`。它写给已经熟悉 nginx 的人：先讲清两个系统的心智模型差在哪，
再给出逐项对应表、会**悄悄改变请求行为**的写法，以及无法表达的能力和替代方案。

读法建议：如果你只想知道"我这套 nginx 配置能不能一比一搬过来"，直接看第 3 节的对应表和第 6 节的
检查清单；如果要评估迁移风险，第 4 节的陷阱清单是本文的核心。

## 1. 先理解三个结构差异

### 1.1 平面资源表，不是 server/location 树

nginx 的配置是一棵树：`http` → `server` → `location`，匹配先选虚拟主机，再在**那个** server 的
location 里找路径。nico 没有这棵树，只有一张平面的 `routes` 列表，每条路由描述"哪个 host +
哪个路径前缀 → 哪个目标"。

- 所有 `http.listeners[]` 条目**共用同一张路由表**：listener 不是虚拟主机，它只决定传输（明文/TLS）
  和报给上游的 `x-forwarded-proto`，不参与选路。
- 匹配时先比 host 等级（写了 `host` 的路由优先于不写的），再在同一等级内比前缀长度。
- 因此"host 命中之后只在该 host 内找"这个 nginx 直觉不成立：一条不限 host 的路由永远可能接住
  一个 host 已被别的路由占用的请求。

### 1.2 两个文件，一个发布单元

| nginx | nico | 边界 |
|---|---|---|
| `nginx.conf` 的 `main`/`events` 段 | `bootstrap.yaml` | 进程级。**不被配置源服务**，远端控制面改不了它 |
| `http` 段里的 `upstream`/`server`/`location`/`map`… | `resources.yaml` | 资源级。由 `config_source` 提供，可热重载 |

`bootstrap.yaml` 是本地文件，启动时读一次、reload 不重读；它决定进程怎么起、监听哪、从哪读配置、
缓存放哪、解析边界多大。`resources.yaml` 才是被 watch/reload 的那份文档。

### 1.3 一次请求读一个快照，reload 是原子替换

nginx 的 `reload` 起新 worker、老 worker 继续服务到连接结束。nico 的模型更简单也更严格：

- 每次发布是一份**不可变快照**（文档 + 编译出的插件链 + LLM 计划 + generation 号）。
- 一个请求从进入到结束只读它开始那一刻的快照；reload 不改变在途请求的路由、限额和预算。
- 新文档任何一处校验或编译失败，**整份**被拒，上一份快照继续服务，同时写入 last-known-good 缓存。
- 不存在的字段被严格拒绝（`deny_unknown_fields`）：拼错字段名不会像 nginx 那样只报 warning 后
  忽略，而是发布失败。

## 2. 进程与监听

| nginx | nico（`bootstrap.yaml`） | 说明 |
|---|---|---|
| `worker_processes` | `runtime.threads` | 1..=256，写给 Tokio runtime builder；`auto` 没有对应写法的等价物，取值需自己算 |
| `listen 8080;` | `http.listeners[].listen: 0.0.0.0:8080` | 端口不能为 0；解析后地址两两不同 |
| `listen 443 ssl;` + `ssl_certificate` | `http.listeners[].tls: {cert_file, key_file}` | 带 `tls` 块即终止 TLS。单证书、启动期加载，换证书要重启 |
| `http2 on;`（TLS） | ALPN 自动协商 | TLS listener 上用 ALPN 协商 `h2`/`http/1.1`，没有开关 |
| `http2 on;`（明文） | `http.h2c: true` | 段级开关，只作用于明文 listener；只有 TLS listener 时写它是启动错误 |
| `ssl_protocols` | 固定 TLS 1.2 / 1.3 | 不可配 |
| `proxy_ssl_trusted_certificate` | `proxy.tls.ca_file` | **完整替换**平台信任库，不是追加；构建 service 时定型，改它要重启 |
| `worker_shutdown_timeout` | `runtime.grace_period_ms` + `graceful_shutdown_ms` | 先等 in-flight 跑完，再在 drain 上限内收尾；drain 不得短于 grace |
| `-s reload`（SIGHUP） | 自动：文件 watch + SIGHUP（`config_source.watch`） | 变化先 debounce，`max_wait_ms` 防止持续变化无限推迟 |
| `-s quit`（SIGQUIT） | `SIGTERM` / `SIGINT` | 没有 `SIGQUIT` 语义；优雅收尾由上面两个窗口决定 |
| `client_body_timeout` / `proxy_read_timeout` | 见 §3 与 T11 | 不是一个字段对一条指令 |
| `access_log` / `log_format` | 无对应 | 结构化日志（`tracing`，stderr）始终输出，形状不可配；请求日志默认 `debug` |
| `error_log` 级别 | `RUST_LOG` | 环境变量，不是文档字段 |

`metrics` 段是可选的独立监听（`/health/live`、`/health/ready`、`/metrics`），默认明文；写上 `tls`
块（与 `http.listeners[].tls` 同形状、同一份读取与配对实现）即只经 TLS 提供，ALPN 只声明
`http/1.1`，证书变更要重启。地址不能与任何数据面 listener 重合。

## 3. 逐项对应表

图例：**等价** = 语义一致；**条件** = 满足说明前提时才一致；**无** = 当前不能表达，见 §5。

| nginx | nico（`resources.yaml`） | 等级 | 前提与差异 |
|---|---|---|---|
| `server { }` | 不存在 | 无 | 没有虚拟主机层级；用 `routes[].host` 表达"这个 host 走这里" |
| `server_name api.example.com` | `routes[].host: api.example.com` | 条件 | 精确匹配、忽略大小写与入站端口；不支持 `*.example.com` 和正则 |
| `default_server` | `host` 省略 | 条件 | 省略 `host` = 匹配任意 host，且**优先级低于**任何写了 host 的路由 |
| `location /api/ { }` | `routes[]` + `path_prefix: /api/` | 条件 | 字节前缀，无规范化、无 `=`/正则/`^~`，不自动 301 补尾斜杠 |
| `location = /health` | 无 | 无 | 只有前缀匹配 |
| `location ~ ^/v\d+/` | 无 | 无 | 只有前缀匹配 |
| `proxy_pass http://upstream;` | `routes[].upstream: <name>` | 条件 | 路径与 query 原样转发，前缀不剥离 |
| `proxy_pass http://upstream/internal/;` | 无 | 无 | 没有 URI 替换；见 §4 T3 |
| `upstream backend { server 10.0.0.7; }` | `upstreams[].endpoints[]` | 条件 | `host` 是逻辑身份（`Host`/SNI），`address` 是连接目标 |
| `server ... weight=3` | `endpoints[].weight: 3` | 条件 | 只有 `weighted_round_robin` 读它；`round_robin` 不读 |
| 默认轮询（加权） | `strategy: weighted_round_robin` | 条件 | 要复现 nginx 默认，必须显式写这个值 |
| `least_conn` | `strategy: least_conn` | 条件 | nico 比**在途请求数**、平票轮转、不加权；nginx 比活跃连接且考虑权重 |
| `ip_hash` | `strategy: ip_hash` | 条件 | 键不同：完整地址 + endpoint 名做会合哈希；nginx IPv4 取前三段 |
| `max_fails=3 fail_timeout=30s` | `max_fails` / `fail_timeout_ms` | 条件 | nico 是连续失败 + 冷却，不是"窗口内累计"；默认 3 / 30000ms（nginx 默认 1 / 10s） |
| `max_fails=0` | `max_fails: 0` | 等价 | 两者都表示永不剔除（nico 仍计数并记录） |
| `proxy_next_upstream error timeout` | 无 | 无 | traffic 路由**一次请求只有一次尝试**；失败即 502 |
| `proxy_set_header Host $host` | 自动 | 无（字段） | `Host` = upstream 的 `host`（非默认端口才带端口），写死不可改 |
| `proxy_set_header X-Forwarded-For` | 自动 | 无（字段） | 在已有值后**追加** peer 地址 |
| `proxy_set_header` 其他头 | 无 | 无 | 没有任意请求头改写 |
| `add_header` / `more_set_headers` | 无 | 无 | 错误响应的头是封闭集合；普通响应透传上游头（`content-length` 由网关重算） |
| `return 403;` / `error_page` | 无 | 无 | 拒绝只由插件和错误映射产生 |
| `rewrite ^/api/(.*) /$1 break;` | 无 | 无 | 见 §5 |
| `limit_req zone=api burst=20 nodelay;` | `plugins[]` + `kind: rate_limit` | 条件 | 即时令牌桶，不排队，超限 429（nginx 默认 503）；见 §4 T9 |
| `limit_req_status 429` | 固定 429 | 等价 | 拒绝码不可配 |
| `limit_conn addr 10;` | 无 | 无 | 没有并发连接/请求限额 |
| `allow 10.0.0.0/8; deny all;` | 无 | 无 | 没有 CIDR 访问控制 |
| `auth_basic` + `auth_basic_user_file` | 无 | 无 | 见 §5 |
| `auth_request /auth;` | 无 | 无 | 没有外部子请求鉴权 |
| 第三方 JWT 模块 | `kind: jwt` | 条件 | 只校验 HS256 Bearer，`exp` 必填 |
| 自定义 API key 校验 | `kind: api_key` | 条件 | 常量时间比较；默认读 `authorization` 并剥 `Bearer` |
| `client_max_body_size 1m;` | `request.max_body_bytes` + `routes[].max_body_bytes` | 条件 | 语义不同：route 只能收紧，写大直接拒绝发布；`0` 表示"只收空体" |
| `client_body_buffer_size` | 无 | 无 | 没有缓冲区大小旋钮 |
| `proxy_request_buffering on` | `plugins[].buffers_body: true` | 条件 | 只在这条路由引用了该插件时；请求头仍然先发 |
| `proxy_buffering on` | 无 | 无 | 普通响应逐 chunk 转发；只有 convert 非流式应答整体缓冲 |
| `proxy_read_timeout 60s` | `request.timeout_ms` + route `timeout_ms` / `ttft_timeout_ms` | 条件 | nico 是"总预算 + 每次读上限"，不是"两次读之间"的独立计时 |
| `proxy_connect_timeout` | 总预算内 | 无（字段） | 连接超时取本次请求的剩余预算 |
| `resolver` / 动态 DNS | 发布期解析一次，展开为逐地址成员 | TTL 后台重解析、权重解析 | 逐地址健康与 failover；地址集合随 reload 更新，请求路径不做域名查询 |
| `upstream ... keepalive 32;` | 无 | 无 | 连接复用由数据平面管理，没有公开参数 |
| `proxy_http_version 2;` | 无 | 无 | 上游固定 HTTP/1.1 |
| `ssl_verify_client`（mTLS） | 无 | 无 | 只有 TLS 终止 |
| `proxy_intercept_errors` | 无 | 无 | 上游错误直接透传 |

## 4. 会改变行为的迁移陷阱

每一条都是"看起来能搬，搬完行为不一样"的具体点。T 编号在 §6 的检查清单里被引用。

### T1｜写了 `host` 不代表隔离了整个入口

直觉：既然 `api.example` 有自己的路由，其他 host 的规则不会接住它的路径。

实际：路由表是平的。设路由 A 为 `host: api.example, path_prefix: /admin/`，B 无 host、前缀
`/public/`。请求 `Host: api.example` + `GET /public/x` **仍然命中 B**。更反直觉的是 host 等级优先于
前缀长度：限定 host 的 `/` 会战胜不限 host 的 `/api/v1/`。

要隔离入口，只能给每条路由都写上 `host`，或者拆成多个进程；nico 当前没有"每个 listener
一张表"的能力。

### T2｜`path_prefix` 是字节前缀，不是 nginx 的 location

三个差异，都会在线上表现为"路由选错"或"404/200 不对"：

- `/api` 匹配 `/apix`；`/api/` 不匹配 `/api`。
- 匹配器不做 URI 规范化：`%2F`、`.`/`..`、连续斜杠都不会被展开或合并，nginx 则是先归一化再匹配
  （Pingora 在把 URI 交给匹配器之前做过什么，见第 8 节）。
- `location /api/` + 带 URI 的 `proxy_pass` 对"恰好是 `/api`"的请求返回 301；nico 只会 404
  （不匹配）或原样转发（匹配）。

### T3｜没有 URI 替换，前缀不会被剥离

`proxy_pass http://backend/internal/;` 在 nginx 里把命中的 location 前缀换成 `/internal/`。nico 没有
这个能力：`/api/x` 到上游还是 `/api/x`。需要改写时，要么让上游接受原路径，要么在前面放一层能做
rewrite 的代理。不要试图用"写一条更长的 `path_prefix`"绕过去——匹配器不改写。

### T4｜路由列表顺序不是覆盖手段

nginx 可以在不同 server 里写同名 location，或靠 `location` 优先级表达意图。nico 里
`(host, path_prefix)` 完全相同就是**重复规则**：发布期直接拒绝整份文档，不存在"后面的覆盖前面的"。
迁移时的重复块必须先合并，不能靠顺序保留语义。

### T5｜`round_robin` 不读 `weight`

nico 的 `round_robin` 严格按文档顺序轮转，权重写了也不影响选择；只有 `weighted_round_robin` 消费
权重（平滑加权，`3:1` 交错为 `a,b,a,a`）。把 nginx 的默认加权轮询搬过来，要么显式写
`weighted_round_robin`，要么接受均分。

### T6｜`least_conn` 比的不是连接数

nico 比的是**在途请求数**，平票按轮转，不看权重。HTTP/2 复用、keepalive 池都会让"连接数"和
"请求数"脱钩；不要假设两个同名策略会做同样的选择。

### T7｜`ip_hash` 的键不一样

nico 用**完整** peer 地址 + endpoint 名做 FNV-1a 会合哈希；nginx 的 `ip_hash` 对 IPv4 只取前三段，
对 IPv6 取完整地址。迁移后粘性映射会整体变化，且 endpoint 改名会重排。

### T8｜失败计数是"连续"的，不是"窗口内"

nico 的 `max_fails` 是**连续失败**次数阈值，冷却 `fail_timeout_ms` 后在冷却期满只放一个试探请求：
成功即清零完整回归，失败重新计时。任何被接受的应答（含 4xx）都清零。nginx 的口径是"`fail_timeout`
期间内的失败数"，两者在同一故障序列下会给出不同的剔除时点。

另外：**traffic 路由没有同请求 failover**。nginx 的 `proxy_next_upstream` 会让当前请求换一台机器重试；
nico 选定成员后这次请求就留在它上面，失败即回；整组都被剔除时直接 `502 upstream_unreachable`，
不会从刚判定失败的成员里再挑一个。LLM 路由的 `retry_budget`/`fallback` 是**另一套**机制，只对
`convert` 路由有效，不适用于普通上游。

### T9｜`limit_req` 不是漏桶，桶也不跨路由共享

| 维度 | nginx `limit_req` | nico `rate_limit` |
|---|---|---|
| 算法 | 漏桶，超出速率的请求默认**排队延迟** | 即时令牌桶，不排队，直接拒绝 |
| 默认拒绝码 | 503（`limit_req_status`） | 429 `rate_limited` + `retry-after` |
| 初始状态 | 空 | burst 满 |
| 共享范围 | 同 `zone` + 同 key 共享 | `(route_id, 实例名, subject\|ip)`；**同实例挂两条路由 = 两个桶** |
| 跨副本 | `zone` 在共享内存，多 worker 共享 | 进程内，多副本不共享 |

想跨路径共享一份额度，唯一办法是把实例放进 `global_plugins`（那里 route_id 为空），但全局与路由
的两次限流是**叠加**关系；而且只有同一作用域里排在它前面的认证插件建立了身份，全局限流才按
subject 分桶，否则回退到调用方地址哈希。

### T10｜数值 `0` 和"写大"的方向

- `client_max_body_size 0` = 关闭检查（不限）；nico 的 `max_body_bytes: 0` = **只接受空体**，任何
  非空请求得 `413 body_too_large`。
- route 的 `max_body_bytes` / `max_response_bytes` **大于**文档值不是"放宽"，是发布期拒绝整份文档。
- route 的 `timeout_ms` 大于 `request.timeout_ms` 会被接受，但运行时**截断**到文档值。
  想要两分钟预算，必须调高 `request.timeout_ms`——那会同时放宽所有没写自己预算的路由。

### T11｜`ttft_timeout_ms` 不是"只等首 token"

名字说的是首 token，实际约束的是**每一次** upstream 读（含 body/chunk 之间的间隔），流式与非流式
都适用。它只在 provider 路由上合法，普通 upstream 路由写它会发布失败。真正的"只等首 token"超时
目前没有。

### T12｜`buffers_body` 只缓冲请求体，而且不是"body 到齐才建连接"

启用 `buffers_body: true` 的插件会让数据平面扣住请求体、累积到齐后一次性交给插件：
**请求头仍然先发**，上游无法把"客户端上传慢"和"插件慢"分开看；内存约为
`request.max_body_bytes × 并发缓冲上传数` + 插件返回值。

响应侧是另一件事：插件接口永远拿不到完整响应（响应可能是无限流）；只有 convert 路由的**非流式**
应答由前端整体缓冲，受 `request.max_response_bytes` 约束——超长时声明的长度在写头之前就 502
（`upstream_response_too_large`），未知长度的 chunked 应答在 body 阶段才被截断，此时状态行已经发出。

### T13｜没有 realip：身份与限流读 socket peer

nico 不做"信任上游代理声明的客户端地址"这件事：`ip_hash`、`identity: ip`、无身份时的限流回退都用
socket peer 地址。`X-Forwarded-For` 只被**追加**，从不被读取为身份来源。迁移时如果依赖
`set_real_ip_from`，前置代理后的限流粒度会整体变化。

### T14｜`enabled: false` 是全局生效

nginx 里把某个 `if`/`location` 注释掉只影响那一处。nico 的插件实例一旦 `enabled: false`，
**所有**引用它的地方都停止编译和运行；反过来，一个写坏的、无人引用的 `enabled` 实例仍会让整份
文档发布失败。

### T15｜没有 `return`、`error_page`、`add_header`、任意 `proxy_set_header`

这些"在代理里写逻辑"的指令都没有对应。拒绝只能由插件产生，且只有实例的 `error_response` 能改
拒绝的形状（`status`/`content_type`/`body`，`code` 与 `x-nico-error-code` 不可覆盖）；没有按条件
`return` 或自定义错误页的机制。

## 5. 没有对应能力时的替代

| 需要 | 现状 | 替代 |
|---|---|---|
| rewrite / URI 替换 | 无 | 上游接受原路径；或在 nico 之前放能做 rewrite 的代理 |
| 正则或精确 location | 无 | 拆成多条前缀路由，接受无法表达的场景 |
| 按 listener 隔离（真虚拟主机） | 无，全 listener 共享一张表 | 给路由都写上 `host`；或开多个进程 |
| 同请求 failover（traffic） | 无 | 让上游自己做重试；或接受 502 由调用方重试 |
| 并发限额（`limit_conn`） | 无 | 前置代理；或写自定义插件（需代码） |
| 跨 route / 跨副本共享限额 | 无 | 放 `global_plugins`（跨 route 但不跨副本）；跨副本需外部存储，接口未暴露 |
| 可信代理地址（realip） | 无 | 前置代理自己做访问控制 |
| `auth_basic` / 外部子请求鉴权 | 无 | 换成 Bearer（`jwt` / `api_key`）；或前置代理完成认证后带身份头进入 |
| CIDR allow/deny | 无 | 前置代理或防火墙 |
| 通用请求/响应缓冲旋钮 | 部分 | 请求侧用 `buffers_body`；响应侧只有 convert 非流式 |
| 任意请求/响应头改写 | 无 | 上游自己处理；或在 nico 之前/之后加一层 |
| SNI 多证书 / mTLS / 证书热加载 | 无（单证书、启动期加载；数据面与 `metrics` 监听同一套规则） | 需要这些能力时前面放一层做 TLS 终止的代理；单纯给自报面加密写 `metrics.tls` 即可 |
| 同一端口自动识别明文与 TLS | 无 | 拆成两个 listener |
| 上游连接池调优 | 无公开参数 | — |
| 动态 DNS（TTL 后台重解析） | 不做：发布期解析一次，DNS 变更随 reload 生效 | 用稳定的单地址（如负载均衡 VIP） |
| 上游 error_page 统一页面 | 无 | 调用方侧处理 |
| 日志格式定制 | 无 | 采集 stderr 结构化日志后自行转换 |

## 6. 迁移检查清单

按顺序做，每一步都能独立验证：

1. **列出真正的虚拟主机边界**（T1）。如果 nginx 配置靠 `server_name` 隔离入口，先决定是给每条路由
   补 `host`，还是接受"全 listener 共享一张表"。
2. **把 location 收敛成前缀**（T2）。删掉正则与 `=`，补齐尾斜杠，确认没有依赖 URI 规范化。
3. **确认没有依赖 URI 替换**（T3）。有的话，先在上游或前置代理解决，再迁移。
4. **合并重复规则**（T4）。构造好文档后用一台测试实例发布一次——重复的 `(host, path_prefix)`
   会在这一步被拒。
5. **重写 upstream 选择**（T5–T7）。把默认加权轮询改成 `weighted_round_robin`；逐条核对
   `least_conn`、`ip_hash` 的键是否可接受。
6. **重写健康与重试**（T8）。删掉 `proxy_next_upstream` 的假设，接受"失败即 502"；按连续失败语义
   重新决定 `max_fails` / `fail_timeout_ms`（默认 3 / 30000ms）。
7. **重写限流**（T9）。确认每个额度的共享范围；需要跨路径时改放 `global_plugins`；期望 429 而非 503；
   预计流量峰值时确认 burst 初始状态是满的。
8. **重算请求边界**（T10–T12）。`max_body_bytes: 0` 不是不限；route 值只能收紧；`ttft_timeout_ms`
   管的是每次读；两个缓冲语义不要混。
9. **确认身份来源**（T13）。前置代理后的 `identity: ip` 得到的是代理地址。
10. **发布后跑这 8 个请求**，对比迁移前后的状态与目标：
    - 少尾斜杠（`/api` vs `/api/`）与超集前缀（`/apix`）
    - 命中 host 但路径属于另一条路由
    - 未知 host + 已知路径
    - 限流触发（看 429 与 `retry-after`，而不是 503）
    - 空 body 与非空 body（确认 `0` 的语义方向）
    - 单成员宕机（看 502，而不是换成员后 200）
    - 慢上传（看 408 `request_timeout`）
    - 慢响应（看 504 `upstream_timeout`）

## 7. 一个完整的等价示例

nginx：

```nginx
upstream api_fleet {
    server 10.0.0.7:8080 weight=3;
    server 10.0.0.8:8080 weight=1;
    keepalive 32;
}

server {
    listen 443 ssl;
    server_name api.example.com;
    ssl_certificate /etc/nginx/tls/fullchain.pem;
    ssl_certificate_key /etc/nginx/tls/key.pem;

    location /api/ {
        proxy_pass http://api_fleet;
        proxy_set_header Host $host;
    }
}
```

nico — `bootstrap.yaml`：

```yaml
config_source:            # 必填：进程要读的那份文档从哪来
  type: local
  path: /etc/nico/resources.yaml

http:                     # 必填：至少一个 listener
  listeners:
    - listen: 0.0.0.0:8443
      tls:
        cert_file: /etc/nico/tls/fullchain.pem
        key_file: /etc/nico/tls/key.pem
metrics:
  listen: 127.0.0.1:9090
```

nico — `resources.yaml`：

```yaml
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576   # nginx 默认 client_max_body_size 1m 的对应写法；注意 0 的含义相反
  timeout_ms: 60000         # 总预算，涵盖上传、上游处理、响应；nginx 没有这个概念，取它的 I/O 超时作上限

# 四个资源节都是必填的：可以为空，省略会被当作 schema 错误。
providers: []
plugins: []

upstreams:
  - name: api-fleet
    scheme: http
    host: api.example.com   # Host 与 SNI 用这个名字
    port: 8080
    strategy: weighted_round_robin   # nginx 默认就是加权轮询，必须显式写
    max_fails: 3
    fail_timeout_ms: 30000
    endpoints:
      - name: a
        address: 10.0.0.7
        weight: 3
      - name: b
        address: 10.0.0.8

routes:
  - id: api
    host: api.example.com   # nginx 的 server_name；不写就等于 default_server
    path_prefix: /api/      # nginx 的 location 前缀；不会被剥离
    upstream: api-fleet
    timeout_ms: 30000       # 只能收紧 request.timeout_ms
```

差异清单（这几条是刻意的，不是遗漏）：连接池没有 `keepalive 32` 对应参数；`Host` 会带端口
（`api.example.com:8080`），因为 8080 不是 `http` 的默认端口，而 nginx 的 `proxy_set_header Host
$host` 不带端口——上游按 vhost 校验 `Host` 时要注意这一行；`round_robin` 的默认值在这里被显式改成
`weighted_round_robin`；失败即返回，不会在同一次请求里换成员。

## 8. 证据与验证边界

本文的行为描述来自当前源码与仓库内测试，不是与 nginx 实进程的差分结果。

- 已由测试固定：路由的 host 等级/前缀优先级、重复规则拒绝、四种选择策略、剔除与回归、限流分桶与
  429、body 上限与 413、超时的 408/504 分界、双 listener 下的 `x-forwarded-proto`、TLS 启动期加载。
- 由代码路径确定、但未逐项实测：特殊 URI（编码斜杠、点段、连续斜杠）在 Pingora 完整矩阵下的表现、
  所有 I/O stall 的定时精度、插件返回体扩张时的峰值内存。
- 未经任何验证：与具体 nginx 版本的 wire 级等价性。nginx 侧语义以其官方文档为准（文档随版本滚动，
  本文不承诺某个具体版本）。

发现本文与实现不一致时，以源码与测试为准，并修正本文。
