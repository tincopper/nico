# nico —— 统一流量 + AI 网关

`nico` 是一个用 Rust 编写的统一网关：一条 HTTP 数据平面既做普通流量代理，也做 LLM 协议转换。
它用一个本地配置文件声明路由、上游、LLM provider 和策略插件；启动后按配置转发请求，并在配置
变化时热重载，失败时保留上一份可用配置继续服务。

> **当前状态**：核心数据平面与可靠性核心可用 —— 路由、插件、LLM 转换、重试 / 熔断 / 降级、优雅停机、
> 可选的自报面（readiness 端点 + metrics exporter）。详见[可观测性](#可观测性)。

## 特性

- **普通流量代理**：按 `host` / `path_prefix` 路由到普通 HTTP 上游。
- **LLM 协议转换**：在 OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Gemini
  `generateContent` 之间转换 —— 客户端说一种协议，后端说另一种。
- **内置策略插件**：HS256 JWT 认证、静态 API Key、令牌桶限流。
- **热重载**：监听配置文件变化，debounce 后原子发布；配置错误不会打断服务，上一份可用配置继续
  生效，并写入 last-known-good 缓存。
- **弹性**：路由级重试预算、显式 fallback 链、provider 熔断器。
- **单一错误出口**：所有失败映射为稳定的 HTTP 状态码 + `x-nico-error-code`，响应体不泄露内部细节。
- **可选的运维面**：默认关闭的独立监听，提供 liveness / readiness 与 Prometheus 指标；关闭时不装
  recorder，请求路径上不产生额外开销。

## 快速开始

### 前置

| 工具 | 说明 |
|---|---|
| Rust | `rust-toolchain.toml` 固定 `1.99.0`，首次 `cargo` 调用时 rustup 自动安装 |
| `curl`（支持 HTTP/2） | 联调 |

完整开发工具链（`cargo-deny`、`lefthook`、nightly rustfmt）见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。

### 运行

示例配置需要三个环境变量（凭证只以 `env:` / `file:` 引用，绝不写进配置文件）：

```bash
JWT_SECRET=local-development-secret \
UPLOAD_KEY=local-upload-key \
OPENAI_API_KEY=local-provider-key \
  cargo run -p nico-server -- --bootstrap config/bootstrap.yaml
```

构建后也可以直接运行二进制：

```bash
cargo build -p nico-server
JWT_SECRET=... UPLOAD_KEY=... OPENAI_API_KEY=... \
  ./target/debug/nico --bootstrap config/bootstrap.yaml
```

命令行只有两种形式：

```
nico --bootstrap <path>   # 启动网关
nico --help               # 打印用法
```

默认监听 `0.0.0.0:8080`（见 `config/bootstrap.yaml` 的 `http.listen`）。

### 验证

下面两条只是请求形状。示例配置里的 `api-backend` 指向 `127.0.0.1:8080`（也就是网关自己），
`openai-primary` 指向真实的 OpenAI API 但用的是占位 key —— 两者都是**占位配置**，请替换成你
自己的 upstream / provider 再跑端到端。

```bash
# 普通上游路由（/api/，插件链：public-jwt → api-quota）：需要 Bearer token
b64() { base64 | tr '+/' '-_' | tr -d '='; }
header=$(printf '{"alg":"HS256","typ":"JWT","kid":"k1"}' | b64)
exp=$(( $(date +%s) + 3600 ))   # exp 必填：无过期的 token 被视为永久凭证而拒绝
payload=$(printf '{"sub":"demo","iss":"https://id.example","aud":"api","exp":%s}' "$exp" | b64)
sig=$(printf '%s.%s' "$header" "$payload" \
  | openssl dgst -sha256 -hmac "$JWT_SECRET" -binary | b64)
curl -sS -H "Authorization: Bearer $header.$payload.$sig" http://127.0.0.1:8080/api/x

# LLM 路由（/v1/chat/，无插件）：客户端说 OpenAI Chat，转发给 openai-primary
curl -sS http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"fast","messages":[{"role":"user","content":"hi"}]}'
```

## 配置

网关有两个文件：

| 文件 | 作用 | 是否可被远端改写 |
|---|---|---|
| `bootstrap.yaml` | 进程级：配置源、缓存、监听地址、drain 窗口、解析边界 | 否，仅本地 |
| `resources.yaml` | 资源级：provider、upstream、route、plugin | 由配置源提供（当前仅本地文件） |

`bootstrap.yaml` 在任何配置源存在之前读取，因此它永远不被配置源服务 —— 远端控制面无法改写进程
读配置的位置或缓存目录。

### bootstrap.yaml

```yaml
config_source:
  type: local            # 当前唯一支持的类型
  path: ./config/resources.yaml
  max_bytes: 1048576
  watch: notify+signal   # notify | signal | notify+signal

debounce_ms: 200         # 变化事件合并窗口
max_wait_ms: 1000        # 变化持续时，发布最多被推迟多久
cache_dir: ./.tmp/cache
allow_bootstrap_cache: false   # 仅当启动失败且显式允许时才回退到缓存

schema_bounds:           # 可选，省略的字段取默认值
  max_routes: 1024
  max_upstreams: 256
  max_providers: 64
  max_plugins: 256
  max_nodes: 100000
  max_depth: 32

http:
  listen: 0.0.0.0:8080
  h2c: false             # 是否在该监听上接受明文 HTTP/2
  grace_period_ms: 5000
  graceful_shutdown_ms: 30000
  threads: 1

tls:                     # 可选；省略整段 = 使用平台证书库
  ca_file: /etc/nico/roots.pem   # 完整信任根集合（PEM），整体替换平台库

metrics:                 # 可选；省略整段 = 不监听、不装 recorder
  listen: 127.0.0.1:9090
```

要点：

- `watch` 决定配置变化的检测方式；`signal` 之外还监听进程信号。
- `allow_bootstrap_cache: true` 时，若首次发布失败，可从 `cache_dir` 的 last-known-good 快照启动
  （该快照被标记为 stale）。健康进程的行为不受影响。
- `http` 的每一项都可省略（取内置默认值），但写出来的值会在启动时校验，不会被默认值吞掉。
- `metrics` 整段省略即关闭自报面（默认）。写出来后 `listen` 必填；与 `http.listen` 同址、端口为 0、
  或端口无法绑定都会让启动失败 —— 一个存在但渲染不出东西的自报面是运维最难排查的形态。
- `tls.ca_file` 是**完整信任根集合，整体替换平台证书库，不是追加**。要同时信任公网与私有 CA，
  请把两者拼进同一个文件。省略整段即用平台库；`SSL_CERT_FILE` / `SSL_CERT_DIR` 仍由底层 TLS
  provider 读取，属部署手段，不是本网关的配置项。
- 信任根在**构建 service 时定型**：改 CA 需要重启，reload 只换文档快照。启动时会先读一次这个来源
  （平台库或 `ca_file`），读不到、为空、或不是证书集合都会让进程拒绝启动，而不是带着空信任集服务。
- 证书校验与主机名校验始终开启，文档无法关掉。

### resources.yaml

```yaml
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576   # 必填；0 表示不接受请求体
  timeout_ms: 30000         # 必填；1..=600000
  # max_response_bytes: 8388608   # 可选，默认 8 MiB；路由只能收紧

providers: []   # 必填节，可为空
upstreams: []   # 必填节，可为空
routes: []      # 必填节，可为空
plugins: []     # 必填节，可为空
global_plugins: []   # 可选；在路由之前对每个请求运行
```

> `providers` / `upstreams` / `routes` / `plugins` 是**必填节**：可以为空，但省略会被当作 schema
> 错误，以免截断的文档看起来像一份可用配置。

### 凭证引用

密钥只以引用形式出现，绝不写值：

- `env:VAR` —— 从环境变量读取；
- `file:/path` —— 逐字节读取文件（`echo secret > file` 留下的换行也算密钥的一部分）。

凭证在配置发布时解析；为空或不可读会**拒绝整份文档**，上一份可用配置继续服务。

## 路由

每条路由恰好指定一个目标：`upstream`（普通上游）或 `provider`（LLM）。

```yaml
routes:
  - id: api-public-v1
    host: api.example.com   # 可选；省略则匹配任意 host
    path_prefix: /api/
    upstream: api-backend
    timeout_ms: 30000        # 可选，受 request.timeout_ms 约束
    max_body_bytes: 1048576  # 可选，受 request.max_body_bytes 约束
    plugins: [public-jwt, api-quota]
```

**匹配优先级**（从高到低）：

1. 指定了 `host` 的路由只匹配该 host（大小写不敏感、忽略端口）；未指定 `host` 的路由匹配任意 host。
2. 限定 host 的路由优先于不限 host 的路由，与前缀长短无关。
3. 同一 host 等级内，`path_prefix` 越长越优先。
4. host 等级与前缀长度都相同时，文档中靠前的路由胜出。

`path_prefix` 按字节比较：`/api/` 不匹配 `/api`，而 `/api` 匹配 `/apix`。路由选择**只读请求头**，
不读请求体。

## 插件

插件是“命名实例”，由路由按名字引用（或列入 `global_plugins`）。一个实例不是 kind 别名：两条路由
需要不同参数，就写两个同 kind 的实例。

```yaml
plugins:
  - name: public-jwt
    kind: jwt
    config: ...
  - name: api-quota
    kind: rate_limit
    enabled: true
    config: ...
  - name: upload-guard
    kind: api_key
    buffers_body: true
    config: ...
```

| 字段 | 说明 |
|---|---|
| `name` | 实例名，路由/日志/审计引用它 |
| `kind` | `jwt` / `api_key` / `rate_limit` 之一 |
| `enabled` | 默认 `true`；`false` 表示声明保留但停止编译/运行 |
| `buffers_body` | 请求侧“整体缓冲”模式：数据平面扣住所有 chunk，把完整 body 交给 `on_request_body`，再转发插件返回的内容 |
| `error_response` | 可选，覆盖拒绝时的 `status` / `content_type` / `body`（`code` 不可覆盖） |
| `config` | 该 kind 自己的配置，发布时解析 |

> `buffers_body: true` 会让该路由整体缓冲：上游要等客户端最后一个字节到达才收到请求，内存为
> `request.max_body_bytes` × 并发缓冲上传数。因此它是逐实例 opt-in。响应永远不可能被缓冲（可能是
> 无限流）。

### jwt

校验 HS256 Bearer token，并在验证通过后建立身份。

```yaml
- name: public-jwt
  kind: jwt
  config:
    keys:
      - kid: k1                 # 可选，写入日志；不可重复
        secret_ref: env:JWT_SECRET
    issuer: https://id.example   # 可选，要求 iss 匹配
    audience: api                # 可选，要求 aud 匹配
    # leeway_seconds: 30        # 可选，默认 30，exp/nbf 的时钟偏移
```

- `keys` 不可为空；`secret_ref` 必须是 `env:` / `file:` 引用。
- **`exp` 必填** —— 无过期的 token 等于永久凭证，会被拒绝；`nbf` / `iss` / `aud` 按配置校验。
- 校验 `exp` / `nbf` 时应用 `leeway_seconds`（默认 30 秒）。
- 缺凭证 → `401 auth_missing`；凭证不被接受 → `401 auth_invalid`（故意不区分“过期/签名错/未知
  key”，避免枚举密钥）。

### api_key

从固定 header 读取静态密钥，常量时间比较。

```yaml
- name: upload-guard
  kind: api_key
  buffers_body: true
  config:
    header: x-upload-key        # 可选，默认 authorization
    keys:
      - name: uploader           # 认证成功后的身份
        secret_ref: env:UPLOAD_KEY
```

- 默认 header 是 `authorization`，值取 `Bearer <key>` 的 key 部分。
- 自定义 header（如 `x-upload-key`）按原样匹配，**不带** scheme 前缀。
- `keys` 不可为空；`name` 会成为请求身份与日志字段。

### rate_limit

令牌桶限流，桶表有界（LRU），跨 reload 保留。

```yaml
- name: api-quota
  kind: rate_limit
  config:
    burst: 20            # 桶容量
    per_second: 5        # 每秒补充
    max_keys: 10000      # 可选，默认 10000，上限 100000
    identity: subject    # 可选，subject | ip，默认 subject
```

- 桶按 `(route_id, 实例名, subject|ip)` 分隔。
- 默认按认证插件建立的身份分桶；`identity: ip` 按客户端地址哈希分桶。
- 超限 → `429 rate_limited`，并带 `retry-after`（秒）。

## LLM 路由

provider 路由在客户端协议与 provider 协议之间转换：`protocol` 指定客户端协议（必填），`mode`
指定转换方式。

```yaml
providers:
  - name: openai-primary
    kind: openai            # openai | openai_responses | anthropic | gemini
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
    model_map:              # 可选：逻辑名 → 物理模型名
      fast: gpt-4o-mini
    model_allowlist: [gpt-4o-mini, gpt-4o]   # 可选：provider 侧模型白名单
    max_output_tokens: 8192                  # 可选：端点在请求未设时的输出上限
    timeout_ms: 120000                       # 可选
    circuit_breaker:
      failure_threshold: 5                   # 连续失败数，0 表示禁用熔断
      cooldown_ms: 30000                     # 打开后等待探针的时间

routes:
  - id: llm-chat
    path_prefix: /v1/chat/
    provider: openai-primary
    protocol: openai.chat     # openai.chat | openai.responses | anthropic.messages | gemini.generate_content
    timeout_ms: 120000
    max_response_bytes: 4194304   # 只能收紧 request.max_response_bytes
    # retry_budget: 2              # 可选，默认 1（不重试），上限 4
    # fallback: [openai-secondary] # 可选，按顺序尝试，最多 3 个
```

### 客户端协议（`protocol`）

| 值 | 方言 |
|---|---|
| `openai.chat` | OpenAI Chat Completions |
| `openai.responses` | OpenAI Responses |
| `anthropic.messages` | Anthropic Messages |
| `gemini.generate_content` | Gemini `generateContent` |

### 转换模式（`mode`）

- `convert`（默认）：解码客户端 body，重新编码给 provider；响应按客户端协议转回。
- `passthrough`：原样转发字节，仅当两侧说同一种协议时有意义。
  - `gemini.generate_content` ↔ `gemini` 的 passthrough 在发布时被拒绝：该方言把 model 和 operation
    放在请求 target 里，固定 target 会把调用方字节转发到路由钉死的 model/operation。
  - Gemini provider 会把 model 钉进 path：其 `model_map` 必须解析到唯一物理模型，解析到其他名字
    的请求回答 `403 model_not_allowed`。

### 重试与降级

- `retry_budget`：单个目标吸收的总尝试次数（含第一次），默认 1，上限 4。可重试失败在预算内从
  **规范化请求**重新编码后重试，绝不重放客户端字节。
- `fallback`：当前目标预算耗尽后按顺序尝试的 provider，最多 3 个；每个成员必须可服务，否则整份
  文档被拒绝。
- 二者只对 `convert` 路由有效（passthrough 没有规范值，也没有失败分类）。
- **硬限制**：请求体超过 64 KiB（Pingora 的重放缓冲区）时，已派发的请求无法重试，这类失败直接
  应答；连接阶段的失败不受此限。

### 熔断

`circuit_breaker` 是端点策略：连续失败达到 `failure_threshold` 打开熔断，`cooldown_ms` 后放一个探针。
`failure_threshold: 0` 禁用熔断（失败仍计数并记录，但不会跳过请求）。重新调整任一值会重置熔断状态
（参数是端点身份的一部分）。

## 错误响应

所有失败由同一个出口映射：一个 HTTP 状态码 + 一个稳定的 `x-nico-error-code` + 一个调用方安全的
message。响应头是封闭集合：`content-type`、`content-length`、`x-nico-error-code`、`x-nico-trace-id`，
以及拒绝携带 challenge 时的 `www-authenticate` 和携带等待时的 `retry-after`。

| 状态 | `x-nico-error-code` | 含义 |
|---|---|---|
| 503 | `not_ready` | 还没有可服务的配置快照 |
| 404 | `route_not_found` | 没有路由匹配 |
| 400 | `invalid_request` | 请求不合法 |
| 413 | `body_too_large` | 请求体超出路由上限 |
| 429 | `rate_limited` | 超过限流阈值（带 `retry-after`） |
| 401 | `auth_missing` | 未提供凭证（带 `www-authenticate: Bearer`） |
| 401 | `auth_invalid` | 凭证不被接受 |
| 408 | `request_timeout` | 客户端未在预算内发完请求 |
| 504 | `upstream_timeout` | 整个请求预算耗尽 |
| 502 | `upstream_unreachable` | 上游不可达 |
| 502 | `upstream_response_too_large` | 缓冲响应超出上限 |
| 502 | `provider_unavailable` | 路由指向 provider 但没有可用前端 |
| 500 | `internal_error` | 数据平面内部故障 |

普通失败渲染固定 JSON 信封：

```json
{"error":{"code":"route_not_found","message":"no route matches the request","trace_id":"..."}}
```

**LLM 路由的失败**由客户端协议自己渲染（调用方看到的是它所说协议的错误文档），状态码与 code 仍
遵循上表，并同样带 `x-nico-error-code`。常见 LLM 稳定码：

| 状态 | code | 含义 |
|---|---|---|
| 422 | `unsupported_capability` | 该路由不支持请求所需的某项能力 |
| 403 | `model_not_allowed` | 路由不服务该模型 |
| 400 | `invalid_request` | 请求对该协议不合法 |
| 502 | `upstream_protocol_error` | 上游返回了网关读不懂的响应 |
| 500 | `provider_encode_failed` / `client_encode_failed` / `upstream_endpoint_invalid` / `internal_error` | 内部故障 |

## 运行时行为

### 热重载

- `watch: notify+signal`：监听文件变化 + 进程信号。
- 变化事件先 debounce（`debounce_ms`），但 `max_wait_ms` 保证变化持续时不会无限推迟发布。
- 发布是原子的：新快照与它编译出的策略链、LLM 计划一起生效；单个请求全程只读一个快照，reload
  不会改变在途请求的路由、限制或 generation。
- 任何校验/编译失败都不会打断服务：上一份可用配置继续服务，并写入 `cache_dir` 的
  last-known-good 缓存。

### 优雅停机

`SIGINT` / `SIGTERM` 触发优雅停机：先等 `grace_period_ms`（默认 5 s）让在途请求跑完，再在
`graceful_shutdown_ms`（默认 30 s）内 drain。整个进程只有一个 shutdown 源（`CancellationToken`），
数据面、配置管道、停机 drain 都由它驱动。

### 构建角色

`nico-server` 有两个 feature，默认 `llm`（all-in-one 网关）：

```bash
cargo build -p nico-server                       # 默认：含 LLM
cargo build -p nico-server --no-default-features --features traffic   # 仅流量
```

`traffic` 角色不编译 `nico-llm`，其注册表为空，声明 provider 路由的文档会在发布时被拒绝 ——
角色是产物属性，不是 bootstrap 里可以写错的字段。

## 可观测性

进程有一个**默认关闭**的自报面，在 bootstrap 的 `metrics.listen` 上独立监听。它不在数据面端口上，
所以配置文档里的任何路由都无法遮蔽它。

| 端点 | 应答 |
|---|---|
| `GET /health/live` | 恒 `200 ok`，**不读任何共享状态** —— 进程能应答即活着 |
| `GET /health/ready` | 能服务时 `200`，否则 `503`；body 是 JSON（`ready`/`generation`/`revision`/`digest`/`stale`/`degraded`/`last_error_category`/`last_reload_unix_secs`） |
| `GET /metrics` | Prometheus 文本（`text/plain; version=0.0.4`） |

liveness 与 readiness 是两个不同的问题，映射到两个不同字段：能回答不等于能服务，而一个坏掉的配置源
也不应该让进程进入重启循环。

**readiness 用的是数据面自己的谓词**（是否已发布一份快照），不是 `ConfigStatus.ready`：后者在从
last-known-good 启动时也是 `true`，拿它做探针会把一个正在服务的进程报成不可服务。

**`degraded` / `stale` 不会让 readiness 变红。** 配置源断开时继续用最后一份可用配置服务是设计行为；
把所有副本摘出 endpoint 会把局部故障放大成全量故障。两个标志都在 body 和 `config_degraded` gauge 里，
需要“不新鲜就别接流量”的部署对该 gauge 告警即可。

非 `GET` 得 `405` 带 `Allow: GET`，未知路径得 `404`，两者都不读共享状态。

### 指标

`/metrics` 输出八个指标族：

| 指标 | 类型 | 标签 |
|---|---|---|
| `config_load_total` | counter | `source`, `result` |
| `config_reload_total` | counter | `result` |
| `config_compile_seconds` | histogram | — |
| `config_watch_reconnects_total` | counter | `source` |
| `config_snapshot_age_seconds` | gauge | — |
| `config_degraded` | gauge | — |
| `gateway_http_requests_total` | counter | `route_id`, `status_class`, `error_category` |
| `gateway_http_request_seconds` | histogram | `route_id` |

`config_compile_seconds` 只包含编译阶段（插件链 + LLM 前端），**不含** last-known-good 缓存写入
—— 后者是磁盘 I/O，把慢盘混进一个叫 `compile` 的数字会把排查引向错误的地方。

`config_watch_reconnects_total` 只在 watch 连续性丢失（收到 `ResyncRequired`）后出现；文件源没有
“重连”，但它是同一个事实：只能全量重读。

### 日志

除了自报面，进程始终输出结构化日志（`tracing`，stderr）：

- 生命周期事件（启动、发布、停机、错误）默认 `info` 级别。
- 每个请求一条记录，默认 `debug` 级别 —— **默认 `RUST_LOG=info` 看不到请求日志**。开启：

```bash
RUST_LOG=nico_http=debug cargo run -p nico-server -- --bootstrap config/bootstrap.yaml
```

请求记录字段：

| 字段 | 说明 |
|---|---|
| `trace_id` | 关联 id；优先回显调用方的 `x-request-id`（短且可打印时），否则用进程内序号 |
| `route_id` / `upstream` | 命中的路由与目标 |
| `config.generation` | 当前配置的 generation |
| `http.method` / `http.status` | 请求方法与状态（取消记为 `499`，仅日志） |
| `body_bytes` / `elapsed_ms` | 请求体字节数与耗时 |
| `error.category` / `error` | 失败分类与原因（仅失败时） |
| `authn` | 身份来源（仅认证后） |
| `plugin` | 失败归因的插件实例（仅策略失败时） |
| `llm.protocol` / `llm.provider` / `llm.model` / `llm.stream` | LLM 路由事实（仅 LLM 路由） |

日志级别语义：正常请求 `debug`，调用方可以修正的拒绝 `warn`，需要运维介入的失败 `error`，取消
`debug`。请求路径禁止 `info`。

## 开发与测试

```bash
# 构建 / 测试 / 检查
cargo build --workspace --all-features
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +nightly fmt --all
cargo deny check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

代码规范见 [`.trellis/spec/backend/`](.trellis/spec/backend/index.md)，提交与 PR 约定见
[`CONTRIBUTING.md`](CONTRIBUTING.md)。

## 目录结构

```text
crates/nico-core/    文档模型、revision、快照、错误分类
crates/nico-config/  ConfigSource 契约、本地文件源、LKG 缓存
crates/nico-http/    Pingora 数据平面：路由、插件链、转发、错误映射
crates/nico-llm/     LLM 协议编解码、provider 适配器、重试/fallback 链
crates/nico-metrics/ 自报面能力层：recorder、Prometheus 渲染、就绪投影（无 socket、无框架）
crates/nico-plugins/ 内置策略：jwt、api_key、rate_limit
crates/nico-server/  启动、reload 管道、进程装配、CLI（二进制 nico）
config/              本地运行的 bootstrap.yaml + resources.yaml
scripts/gates/       git hook 背后的检查
```

## 当前限制

- **配置源**：当前只实现 `type: local`（本地文件）。远端控制面尚未接入。
- **路由匹配**：线性扫描，无编译索引；`path_prefix` 是字节前缀，不做路径归一化。
- **入站 TLS**：数据面的 `listen` 只服务明文 HTTP（可开 `h2c`）；需要在该端口终止 TLS 时，请在
  前面放一个代理。上游方向直接支持 `https`（见 `tls.ca_file`）。
