# nginx 资源配置语义：官方来源摘录

取证日期：2026-11-18。直接读取 nginx 官方在线文档；本文记录资源审查需要的稳定语义，不假设 nico 与 nginx 某个完整版本兼容。官方页面为滚动文档，较新版本或商业能力以原页面说明为准。

## 来源索引

| ID | 官方文档 | 本次核查范围 |
|---|---|---|
| N1 | https://nginx.org/en/docs/http/request_processing.html | server 选择、listen 与默认 server、Host 与 location |
| N2 | https://nginx.org/en/docs/http/ngx_http_core_module.html#location | URI 规范化、前缀/正则/精确匹配、尾斜杠重定向 |
| N3 | https://nginx.org/en/docs/http/ngx_http_core_module.html#client_max_body_size | body 上限、0 的含义 |
| N4 | https://nginx.org/en/docs/http/ngx_http_core_module.html#client_body_timeout | 两次读之间的超时 |
| N5 | https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_pass | 上游 URL、含 URI 与不含 URI 的路径转发 |
| N6 | https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_set_header | Host 默认值、显式覆盖、同层指令与继承 |
| N7 | https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_read_timeout | 上游两次读之间的超时 |
| N8 | https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_request_buffering | 整体请求缓冲与流式上传 |
| N9 | https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_buffering | 响应缓冲与磁盘临时文件 |
| N10 | https://nginx.org/en/docs/http/ngx_http_upstream_module.html | upstream、server 参数、四种相关选择方式 |
| N11 | https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_next_upstream | 换成员条件、失败计数、重试边界 |
| N12 | https://nginx.org/en/docs/http/ngx_http_limit_req_module.html | 请求速率、漏桶、burst/delay/nodelay、状态码、zone |
| N13 | https://nginx.org/en/docs/http/ngx_http_limit_conn_module.html | 并发连接/请求上限 |
| N14 | https://nginx.org/en/docs/http/ngx_http_auth_request_module.html | 认证子请求 |
| N15 | https://nginx.org/en/docs/http/ngx_http_auth_basic_module.html | HTTP Basic 用户名/密码认证 |
| N16 | https://nginx.org/en/docs/http/ngx_http_realip_module.html | 受信代理地址替换与客户端地址 |
| N17 | https://nginx.org/en/docs/dev/development_guide.html#http_phases | 模块注册与请求处理阶段 |

## 1. server 与 location 是两次选择（N1、N2）

- nginx 先根据接收请求的 listen 地址/端口筛选 server，再用请求 Host 匹配这些 server 的 `server_name`。
- 未命中名字时使用该 listen 端口的默认 server；默认项可由 `default_server` 显式设置，否则通常为第一个 server。
- 原文：“a default server is a property of the listen port and not of the server name”。
- 选定 server 后才在它内部选择 location；其他 server 的 location 不参与补位。
- 前缀 location 先记住最长匹配；正则按声明顺序检查，第一个命中获选；`^~` 可阻止后续正则检查；`=` 命中立即结束。
- location 匹配的是规范化后的 URI：`%XX` 解码、处理 `.` / `..`、根据配置合并相邻斜杠；不以 query string 参与 location 匹配。
- 以 `/` 结尾的代理 location 还有特殊行为：请求恰好少了末尾 `/` 时，nginx 可返回补斜杠的 301。

迁移判断：单条带 host 的平面路由不能据此称为完整 server/vhost 实现；“最长前缀”也不足以证明 location 语义相同。

## 2. proxy_pass 的 URI 是路径操作（N5、N6）

```nginx
location /api/ {
    proxy_pass http://backend/internal/;
}
```

此处 `/api/x` 传向 `/internal/x`：包含 URI 的 `proxy_pass` 将规范化 URI 中匹配 location 的部分替换为该 URI。

```nginx
location /api/ {
    proxy_pass http://backend;
}
```

不含 URI 时，原始未改写请求通常保留客户端请求 URI；若已发生 rewrite 等 URI 修改，适用完整规范化 URI规则。正则 location、命名 location、含变量的 proxy_pass 有专门例外，不可推广为统一字符串拼接。

nginx 默认重设 `Host` 为 `$proxy_host`；`proxy_set_header Host ...` 可显式配置。上游组名、连接地址、请求 Host、TLS SNI 是需要分别核对的事实。

迁移判断：仅定义一个上游目标，不意味着支持 `proxy_pass` 后缀 URI、rewrite 或通用 header 配置。

## 3. 上游选择与健康状态（N10、N11）

- 默认 upstream 选择为**加权**轮询；`server ... weight=N` 默认 1。
- nginx 的 `least_conn` 按活跃连接数并考虑权重，平局使用加权轮询。
- nginx 的 `ip_hash` 用 IPv4 前三个八位组或完整 IPv6 地址作为键；不是任意 IP 哈希算法的统称。
- `hash ... consistent` 使用 ketama；即使另一算法同样减少成员增删时迁移量，也不保证键到成员的映射兼容。
- `max_fails` 默认 1；在 `fail_timeout` 窗口内达到失败数才判为不可用。`fail_timeout` 同时是失败统计窗口和不可用持续时间，默认 10 秒。
- 单成员 upstream 的 `max_fails`、`fail_timeout`、`slow_start` 被忽略，该唯一成员不会被视为 unavailable。
- `max_fails=0` 关闭失败尝试计数。
- 失败分类由 `proxy_next_upstream` 等决定；默认 `error timeout`。5xx 并非默认全部计入：`http_500/502/503/504/429` 仅在显式配置时计为失败；403/404 永不计为失败尝试。
- 普通代理可在条件允许时换下一台成员；是否已向客户端写响应、是否已发送非幂等请求等限制重试。`proxy_next_upstream_tries` / `_timeout` 约束尝试次数和时间。
- upstream 的 server 可单独带权重、健康参数、backup/down、Unix socket 等；`zone` 负责多 worker 间共享运行状态。

迁移判断：策略词汇与健康参数同名只能说明概念相近；默认值、统计窗口、状态作用域、单成员特例、失败分类和同请求换成员必须逐项比较。

## 4. 资源上限与超时（N3、N4、N7）

- `client_max_body_size` 的 0 **关闭大小检查**，默认 1 MiB；不能将其他产品的 0 值原样搬过来。
- nginx 可以在 http/server/location 层设置 body 上限；其上下文覆盖机制不能推导出 nico 也允许路由放宽文档总上限。
- `client_body_timeout` 是客户端 body 两次读取之间的时间，默认 60 秒；并非整个上传的总预算。
- `proxy_read_timeout` 是上游响应两次读取之间的时间，默认 60 秒；持续有数据的长流可以持续超过这个值。
- `proxy_send_timeout` 同样约束两次写之间的间隔；`proxy_connect_timeout` 单独约束连接建立。

迁移判断：单个整请求 deadline 不能以“超时”二字当作这些计时器的等价项。

## 5. 缓冲的方向和目的（N8、N9）

- `proxy_request_buffering` 默认 on，先读完整客户端 body 再发送给上游；off 表示随收随发，并改变换成员重试条件。
- `proxy_buffering` 默认 on，指**响应方向**；可借助 buffer 和磁盘临时文件解耦上下游速度。off 时同步转发响应。
- 请求缓冲、响应缓冲、响应体大小上限是三件不同的事情。某个插件要求完整请求 body，不能被解释成通用双向代理缓冲开关。

## 6. 模块、顺序、限流与认证（N12—N17）

### 请求速率

`limit_req` 原文：“The limitation is done using the ‘leaky bucket’ method.” 超速请求默认被延迟，超过 burst 才拒绝；`nodelay` 取消延迟。默认拒绝状态码是 503，可由 `limit_req_status` 改成 429 等。`burst` 是额外突发额度，不能无条件等同于其他算法的桶容量。

`limit_req_zone` 在 http 层定义状态区和键表达式；多个 location 使用同一 zone 时共享该 zone 对同一键的计数。limit_req 仅在当前层没有自己的指令时整体继承上层指令。

### 并发

`limit_conn` 限制按键统计的并发连接；计入已完整读头且正在处理的请求。HTTP/2/3 每个并发请求单独计数。它与请求速率是不同能力，不能映射到一个每秒请求数插件。

### 认证

- `auth_request` 发认证子请求，2xx 放行，401/403 拒绝；不是直接在网关核验 JWT 或静态 key。
- `auth_basic` 实现 HTTP Basic 用户名/密码验证；不能直接替换为 bearer API key。
- 官方开发指南的 HTTP phases（N17）明确：`limit_req` / `limit_conn` 注册在 PREACCESS，`auth_basic` 等认证处理位于随后的 ACCESS，realip 在 POST_READ；指令书写顺序不能改变这些阶段。与其他网关比较时必须核对认证与限流的先后，而不是只比较列表顺序。

### 客户端地址

nginx 的 realip 模块需要 `set_real_ip_from` 声明可信代理，再按 `real_ip_header` / `real_ip_recursive` 替换客户端地址。迁移 IP 哈希或 IP 限流时，应明确 nico 使用的是连接对端还是被信任的转发地址，不能把现有 X-Forwarded-For 原样当作客户端身份。

## 7. 旧调研映射的复核线索

来源：`.trellis/tasks/archive/2026-11/11-17-dual-listener/research/nginx-config-analysis.md` §8。本节保留审查线索，不能替代当前 nico 源码证据。

| 旧调研说法 | 本次复核结果 |
|---|---|
| nico 无 host 路由、server 可直接摊平成 listener | 当前 README 已描述 host 匹配；nginx server 还绑定 listen 与 server_name（N1）。需要回读代码明确平面路由与 vhost 的边界 |
| `limit_req` / `limit_conn` 都对应 rate-limit | N12 与 N13 明确是速率和并发两类能力，不能合并对应 |
| `auth_request` / `auth_basic` 对应 jwt / api-key | N14、N15 是子请求与 Basic 认证；它们只有业务目的相近，协议和执行机制不等价 |
| LB 使用 nginx 词汇，因此策略已对齐 | N10 的 `ip_hash` 键、`least_conn` 权重和默认加权轮询都是具体算法约束；需要逐策略复核 |
| `max_fails` / `fail_timeout` 已概念对齐 | N10 的失败统计窗口与不可用持续时间共用一个值，默认 1 / 10s；需核对 nico 的统计方式与单成员规则 |
| `proxy_set_header` 对应内置 attribution + 插件链 | 内部插件接口能够改头，不等于公开配置已支持任意 header 设置；需要区分内置行为和可配置能力 |

## 审查约束

本文件给 nginx 官方事实与旧映射的复核线索。nico 一侧必须以 `current-resource-contract.md` 的代码证据补齐后才能下等价或调整结论；正式迁移指南与产品变更仍待用户批准的任务边界。
