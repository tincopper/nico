# nico —— 统一流量 + AI 网关

`nico` 是一个用 Rust 编写的统一网关：一条 HTTP 数据平面既做普通流量代理，也做 LLM 协议转换。
它用一个本地配置文件声明路由、上游、LLM provider 和策略插件；启动后按配置转发请求，并在配置
变化时热重载，失败时保留上一份可用配置继续服务。

> **当前状态**：核心数据平面与可靠性核心可用 —— 路由、插件、LLM 转换、重试 / 熔断 / 降级、优雅停机、
> 可选的自报面（readiness 端点 + metrics exporter）。详见[可观测性](#可观测性)。

## 特性

- **普通流量代理**：按 `host` / `path_prefix` 路由到普通 HTTP 上游。
- **上游多实例**：一个 upstream 可写一组成员，按权重 / 活跃连接 / 调用方地址选择，失败成员被动剔除、
  冷却后单探针回归；`Host` 与 SNI 始终取逻辑 host。
- **LLM 协议转换**：在 OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Gemini
  `generateContent` 之间转换 —— 客户端说一种协议，后端说另一种。
- **内置策略插件**：HS256 JWT 认证、静态 API Key、令牌桶限流。
- **热重载**：监听配置文件变化，debounce 后原子发布；配置错误不会打断服务，上一份可用配置继续
  生效，并写入 last-known-good 缓存。
- **弹性**：路由级重试预算、显式 fallback 链、provider 熔断器、上游成员剔除。
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

监听地址由 `http.listeners[]` 决定（`http` 段必填）；`config/bootstrap.yaml` 的示例监听 `0.0.0.0:8080`。

### 容器部署

镜像按 **role** 在构建期决定：`llm`（默认，全功能）或 `traffic`（不编入 `nico-llm`）。
role 是制品属性，不是运行时开关——`traffic` 镜像遇到声明 provider 路由的文档会在发布期拒绝，
并保住上一个已知好的快照。

```bash
docker build --build-arg ROLE=llm     -t nico:llm .
docker build --build-arg ROLE=traffic -t nico:traffic .
docker compose up -d --build          # 本地清单，默认 ROLE=llm
```

镜像内自带一份可运行的样例：`deploy/bootstrap.yaml`（监听 8080、metrics 9090、
LKG 缓存在 `/var/lib/nico/cache`）与 `deploy/resources.yaml`（只有 upstream 路由，两种 role
都能发布）。不带任何卷 `docker run nico:llm` 也能起；compose 把 `./deploy` 只读挂到
`/etc/nico/`，把缓存放进命名卷。

| 端点 | 用途 |
|---|---|
| `http://localhost:8080/` | 数据面 |
| `http://localhost:9090/health/live` | 进程可答即 200 |
| `http://localhost:9090/health/ready` | 首次成功发布后 200 |
| `http://localhost:9090/metrics` | 指标 |

换自己的配置：改 `deploy/` 下两份文件后 `docker compose up -d --build` 会重建镜像，
或直接改挂载路径。`credential_ref: env:...` 的值通过 compose 的 `environment` 注入，不要写进文档。
`ROLE=traffic` 时 `resources.yaml` 不能出现 provider 路由。

上游 TLS 用镜像内的平台根证书（runtime 装了 `ca-certificates`）；私有 CA 挂一个 bundle 并
取消 `deploy/bootstrap.yaml` 里 `proxy.tls.ca_file` 的注释——改这个文件需要重启，reload 不重读它。

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
| `resources.yaml` | 资源级：provider、upstream、route、plugin | 由配置源提供（`local` 文件或 `etcd`） |

`bootstrap.yaml` 在任何配置源存在之前读取，因此它永远不被配置源服务 —— 远端控制面无法改写进程
读配置的位置或缓存目录。

### bootstrap.yaml

```yaml
config_source:
  type: local            # local | etcd（etcd 需编译时开 `--features etcd`）
  path: ./config/resources.yaml
  max_bytes: 1048576
  watch: notify+signal   # notify | signal | notify+signal

  debounce_ms: 200       # 变化事件合并窗口
  max_wait_ms: 1000      # 变化持续时，发布最多被推迟多久
  cache_dir: ./.tmp/cache
  allow_bootstrap_cache: false   # 仅当启动失败且显式允许时才回退到缓存

  schema_bounds:         # 可选，省略的字段取默认值
    max_routes: 1024
    max_upstreams: 256
    max_providers: 64
    max_plugins: 256
    max_endpoints: 64    # 单个 upstream 的成员数上限
    max_nodes: 100000
    max_depth: 32

runtime:                 # 可选；省略整段 = 全默认
  threads: 1
  grace_period_ms: 5000
  graceful_shutdown_ms: 30000

http:                    # 必填；一个条目 = 一个 listener
  h2c: false             # 是否在明文 listener 上接受明文 HTTP/2
  listeners:
    - listen: 0.0.0.0:8080
    # 带 `tls` 块的条目是 TLS listener（单证书、启动期加载）。
    # - listen: 0.0.0.0:8443
    #   tls:
    #     cert_file: /etc/nico/tls/fullchain.pem
    #     key_file: /etc/nico/tls/key.pem

proxy:                   # 可选；省略整段 = 使用平台证书库
  tls:
    ca_file: /etc/nico/roots.pem   # 完整信任根集合（PEM），整体替换平台库

metrics:                 # 可选；省略整段 = 不监听、不装 recorder
  listen: 127.0.0.1:9090
  # 写上 `tls` 块即只经 TLS 提供（单证书、启动期加载、只协商 http/1.1）。
  # tls:
  #   cert_file: /etc/nico/tls/fullchain.pem
  #   key_file: /etc/nico/tls/key.pem

quota_store:             # 可选；省略整段 = token 预算表留在进程内（单副本默认）
  type: redis            # etcd | redis（需编译对应 feature）
  key_prefix: nico/quota # 1..=256 字节，printable ASCII
  on_unavailable: fail-closed   # 必填；fail-closed | fail-open
  # redis:
  #   endpoints: ["rediss://quota.internal:6379"]
  # 或（type: etcd 时）：
  #   endpoints: ["https://etcd-0.internal:2379"]
  #   tls: { ca_file: /etc/nico/etcd-ca.pem }
  #   auth: { user: nico, password_ref: env:ETCD_PASSWORD }
```

要点：

- `watch` 决定配置变化的检测方式；`signal` 之外还监听进程信号。
- `allow_bootstrap_cache: true` 时，若首次发布失败，可从 `cache_dir` 的 last-known-good 快照启动
  （该快照被标记为 stale）。健康进程的行为不受影响。
- `http` 段**必填**：缺席即启动失败，而不是退化成默认的 8080。`listeners` 至少一条，端口不能为 0，
  地址解析后两两不同；写出来的值在启动时校验，不会被默认值吞掉。
- `http.listeners[]` 的一个条目就是一个 listener：带 `tls` 块的条目终止 TLS（证书/私钥启动时读取
  并配对，坏文件拒绝启动；TLS 1.2/1.3、ALPN `h2`/`http/1.1`；变更需要重启，不做热加载），不带的条目
  只服务明文。两种条目可以在同一份配置里并存（TLS-only、纯明文、明文+TLS 都由同一张 schema 表达），
  每个连接报给上游的 `x-forwarded-proto` 由该连接自己的传输决定。
- `tls` 块是**本 listener 出示的身份**；`proxy.tls` 是**上游**握手的信任根，两个方向两个名字。
- `http.h2c` 是段级事实：作用于全部明文条目。只有 TLS 条目时写 `h2c: true` 是启动错误 —— 一个
  listener 只有一种传输，这个开关没有作用对象（TLS listener 走 ALPN 协商 HTTP/2，不受影响）。
- `runtime` 段三个进程旋钮可省略取默认；写出来则在启动时校验（`threads` 1..=256、两个窗口
  1..=300_000、drain 不得短于 grace）。
- `metrics` 整段省略即关闭自报面（默认）。写出来后 `listen` 必填；与任一 `http.listeners[]` 地址
  同址、端口为 0、或端口无法绑定都会让启动失败 —— 一个存在但渲染不出东西的自报面是运维最难排查
  的形态。
- 自报面可选终止 TLS：`metrics.tls` 与 `http.listeners[].tls` 形状相同、读取与配对同一份实现。
  写了 `tls:` 就必须给齐两个文件（空块是配置错误，不会退回明文），ALPN 只声明 `http/1.1`（抓取器
  和探针不需要 h2），证书/私钥在启动期定型、换证书要重启；省略 `tls` 仍是明文。
- `proxy.tls.ca_file` 是**完整信任根集合，整体替换平台证书库，不是追加**。要同时信任公网与私有 CA，
  请把两者拼进同一个文件。省略整段即用平台库；`SSL_CERT_FILE` / `SSL_CERT_DIR` 仍由底层 TLS
  provider 读取，属部署手段，不是本网关的配置项。
- 信任根在**构建 service 时定型**：改 CA 需要重启，reload 只换文档快照。启动时会先读一次这个来源
  （平台库或 `ca_file`），读不到、为空、或不是证书集合都会让进程拒绝启动，而不是带着空信任集服务。
- 证书校验与主机名校验始终开启，文档无法关掉。
- `quota_store` 整段省略即进程内预算表（默认，单副本行为不变）。写出来则选择共享后端并把
  `on_unavailable` 当必填项：缺策略即启动错误，网关不替部署方决定可用性与预算绕过的取舍。
  选定的后端无法构建（feature 未编译、凭据读不到、连接不上）同样是启动拒绝——用错的存储服务
  预算比不启动更糟。详见 [token 限额](#token-限额) 一节。

### config_source：etcd

`type: etcd` 把同一个 key 当作一份完整文档，多端点之间的故障转移由 client 自己做：

```yaml
config_source:
  type: etcd
  endpoints:
    - https://etcd-0.internal:2379
    - https://etcd-1.internal:2379
  key: /nico/resources.yaml
  max_bytes: 1048576

  tls:                                 # 可选；出现即 TLS
    ca_file: /etc/nico/etcd-ca.pem     # 块存在时必填：信任根，整体替换平台库
    # cert_file: /etc/nico/etcd-client.pem      # 可选，与 key_file 成对
    # key_file: /etc/nico/etcd-client-key.pem
    # domain_name: etcd.internal                # 可选；证书校验名，缺省取端点主机名

  auth:                                # 可选；出现即鉴权
    user: nico
    password_ref: env:ETCD_PASSWORD    # 只接受 env: / file: 引用，不写明文
```

- 传输方式在启动期定型：`https://` 端点**必须**配 `tls` 块，`tls` 块**只能**配 `https://` 端点，
  一份列表不能混用两种 scheme —— 猜 scheme 要么把配置文档明文送上网，要么连不上。不带 `tls` 时
  `http://` 与裸 `host:port` 照旧可用。
- 信任根**只来自 `ca_file`**，平台证书库不参与：信任根是启动事实，不是「跑在哪台机器上」的推断。
  换 CA 或换客户端证书需要重启，客户端证书不做热加载。
- 三个文件在校验期读一次、在构建 source 时再读一次：读不到、不是证书集合、证书与私钥不配对，
  都是点名 `config_source.tls.*` 的启动错误（两次读取之间被换掉也算）。
- `password_ref` 只写引用，值在构建 source 时读一次；`file:` 逐字节读，`echo` 留下的换行同样是
  口令的一部分。
- `domain_name` 对该 client 的全部端点生效（一个 client 只校验一个名字）；逐端点的证书名本版不支持。
- 口令错误或端点拒绝凭据是 `EtcdError::Auth`，端点不可达或 TLS 握手失败是 `EtcdError::Connect`；
  两者都没有降级成明文或匿名访问的形态，进程直接拒绝启动。

### quota_store：跨副本共享 token 预算

省略整段时，token 预算表留在进程内——单副本行为，也是此前每个部署的行为。多副本部署写这个段，
让同一 `(route, tenant)` 或 `(route, subject)` 的窗口由所有副本共享：

```yaml
quota_store:
  type: etcd                 # etcd | redis（分别需要 `etcd` / `redis` feature）
  key_prefix: nico/quota     # 1..=256 字节 printable ASCII，所有预算 key 的公共前缀
  on_unavailable: fail-closed # 必填：fail-closed | fail-open

  endpoints:                 # etcd 与 redis 都以 endpoints 列表给定地址
    - https://etcd-0.internal:2379

  tls:                       # 仅 etcd；形状与 config_source.tls 相同
    ca_file: /etc/nico/etcd-ca.pem
  auth:                      # 仅 etcd；password_ref 只接受 env: / file: 引用
    user: nico
    password_ref: env:ETCD_PASSWORD
```

- **每次检查与记账都是后端原子操作**：etcd 用一次 compare-and-put 事务，Redis 用一次 Lua 脚本；
  读-改-写永远在存储内完成，两个副本不会各写半个窗口。并发记账不丢增量。
- **记账幂等**：每次成功转换的请求携带稳定 charge id，后端按 `(namespace, key, charge id)` 去重
  ——网络超时后的重试只计一次账。历史保留最近 64 条 charge id。
- **`on_unavailable` 是部署方的取舍**，两个值都不是网关的默认：
  - `fail-closed`：预算不可确认即拒绝，返回 `503 quota_store_unavailable`——`429` 只表达
    「窗口确实花完了」，存储故障是另一回事；
  - `fail-open`：保持请求可用，接受存储故障期间的超额；放行会计入
    `quota_store_degraded_total{policy}` 指标并输出 `warn` 日志。
  - 两种策略的放行/拒绝都计入该指标，存储健康状况在指标里可见。
- **事后记账语义不变**：用量在 provider 返回后才知，一次请求可以越过预算，拒绝的是下一个
  请求；本节不引入预留，也不承诺零超额硬上限。
- **窗口过期用记录内的时间戳**（etcd）或 TTL（Redis，仅作清理）；跨副本时间统一为 wall-clock，
  进程内单调时钟不出网。
- **存储无法构建即启动拒绝**：feature 未编译、凭据读不到、端点连不上，进程都不启动——
  用错的存储服务预算比不启动更糟。运行期故障按 `on_unavailable` 处理，不打断服务。
- `quota_store_degraded_total{policy}` 计的是**存储故障时被策略处置的请求**，不是故障事件数；
  一个持续宕机的存储会让这个数字随流量增长，这正是「还在降级运行」的信号。

### resources.yaml

```yaml
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576   # 必填；0 表示不接受请求体（不是「不限」）
  timeout_ms: 30000         # 必填；1..=600000，是全文档预算也是每条路由的上限
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

每条路由命名一个目标：`upstream`（普通上游）或 `provider`（LLM）；需要灰度时，以池
（`upstreams` / `providers`）在多个目标间按权重分发。

```yaml
routes:
  - id: api-public-v1
    host: api.example.com   # 可选；省略则匹配任意 host
    path_prefix: /api/
    upstream: api-backend
    timeout_ms: 30000        # 可选；只能收紧 request.timeout_ms
    max_body_bytes: 1048576  # 可选；不能超过 request.max_body_bytes，否则发布期拒绝
    plugins: [public-jwt, api-quota]

  # 池形态（灰度/蓝绿）：一条路由按权重分发到多个 upstream，与单目标 `upstream` 互斥
  - id: api-canary
    path_prefix: /v2/
    strategy: weighted_random   # weighted_random（默认，按权重抽样）| round_robin（文档顺序轮转）
                                # | ip_hash（调用方地址会合哈希：同一调用方稳定落同一 upstream）
    upstreams:
      - upstream: api-stable    # 引用已声明的 upstream；同一池内不得重复
        weight: 9               # 1..=1000；traffic 路由没有兜底链，0 会被拒绝
      - upstream: api-canary
        weight: 1
```

**upstream 池（灰度）**：请求先由路由池选 upstream，再由该 upstream 自己的成员池选机器。路由池的
候选只含**仍有可用成员**的 upstream——某个 upstream 全员被剔时，它的份额自动落到其余成员；全部
不可用回答既有的 `502 upstream_unreachable`。池的轮转位置跨 reload 继承；`ip_hash` 的会合键是
调用方完整地址，无地址的调用方按轮转铺开。请求记录与指标的标签不变：记录的仍是实际服务的
upstream 与 endpoint。

**匹配优先级**（从高到低）：

1. 指定了 `host` 的路由只匹配该 host（大小写不敏感、忽略端口）；未指定 `host` 的路由匹配任意 host。
2. 限定 host 的路由优先于不限 host 的路由，与前缀长短无关。
3. 同一 host 等级内，`path_prefix` 越长越优先。
4. host 等级与前缀长度都相同时，文档中靠前的路由胜出——但这样的两条规则在发布期就被判为重复
   并拒绝整份文档，所以合法文档里不存在需要靠顺序取胜的情形。

`path_prefix` 按字节比较：`/api/` 不匹配 `/api`，而 `/api` 匹配 `/apix`。路由选择**只读请求头**，
不读请求体。列表是平面的：所有 `http.listeners[]` 条目共用这一张表，listener 不参与选择（它不是
虚拟主机）；路由前缀也不会被剥离，`/api/` 路由把原路径转给上游，未命中的“父前缀路由”不提供
插件或限制。

`request` 段与路由字段的约束方式不一样：

| 字段 | 路由值超过文档值时 |
|---|---|
| `max_body_bytes` / `max_response_bytes` | 发布期拒绝整份文档 |
| `timeout_ms` | 仍然发布，但运行时取较小值——写大不会放宽预算 |

超时从请求进入数据面时算一次，命中路由只收紧、不重新计时。

## 上游（`upstreams`）

一个 upstream 是**一个逻辑身份 + 一组成员（endpoint）**。`scheme` / `host` / `port` 永远描述身份：
`host` 是写进 `Host` 头与 TLS SNI 的名字，`scheme` 与 `port` 是成员的默认值。

```yaml
upstreams:
  # 单实例写法：不写 endpoints 等于一个成员，行为与以前完全一致
  - name: api-backend
    scheme: http
    host: 127.0.0.1
    port: 8080

  # 一组成员：address 是连接目标，host 仍是身份
  - name: api-fleet
    scheme: https
    host: api.example.com      # 证书签的是这个名字，不是下面任何一台
    port: 443
    strategy: round_robin      # 可选；默认 round_robin
    max_fails: 3               # 可选；默认 3，0 表示永不剔除
    fail_timeout_ms: 30000     # 可选；默认 30000，1..=600000
    endpoints:
      - name: a               # 名字在所属 upstream 内唯一，是日志与指标里的唯一标识
                              # （同一 upstream 内 address:port 也不得重复）
        address: 10.0.0.7     # 可选；缺省 = upstream 的 host
        port: 8443            # 可选；缺省 = upstream 的 port
        weight: 3             # 可选；默认 1，1..=1000
      - name: b
        address: 10.0.0.8
        weight: 1

  # 主动健康检查（可选；缺省关闭——不写 health_check 的 upstream 只从真实请求学习）
  - name: checked-backend
    scheme: http
    host: 127.0.0.1
    port: 8080
    health_check:
      path: /health              # 必填；`/` 开头，无 query/fragment/空白
      method: GET                # 可选；GET（默认）或 HEAD
      expect: non_5xx            # 可选；non_5xx（默认，4xx 也算存活）或 2xx
      interval_ms: 5000          # 必填；1..=600000
      timeout_ms: 1000           # 必填；1..=600000
      no_traffic_interval_ms: 30000   # 可选；缺省 = interval_ms；上游无真实请求时的检查间隔
      unhealthy_threshold: 2     # 可选；连续失败次数，默认 1，1..=100
      healthy_threshold: 2       # 可选；连续成功次数，默认 1，1..=100
```

| 事实 | 说明 |
|---|---|
| 连接目标 vs 身份 | 连接去 `address:port`，`Host` 与 SNI 用 `host`。同一 vhost 背后多台机器、或同 host 多端口，都能通过证书校验 |
| 域名成员 | `address` 写域名时，**发布期解析一次**，每个解析地址都是独立成员（逐地址健康、least_conn 与 failover）；名字与权重被每个成员继承，流量份额 ∝ 地址数 × weight。解析失败、或展开结果与其它成员撞 `address:port`，都会拒绝发布（LKG 保持服务）。请求路径不做域名查询，DNS 变更随下一次 reload 生效 |
| endpoint 不带 `scheme` | 一个 upstream 只有一种传输方式；混协议是两台不同的服务，写两条 upstream |
| `weight: 0` 被拒绝 | 流量层没有“只兜底”的语义，0 权重等于永不选中但它看起来像个权重 |
| 主动健康检查 | 写 `health_check` 即启用周期探测：每个成员按 `interval_ms` 探测（无真实流量时用 `no_traffic_interval_ms`），连续失败达到 `unhealthy_threshold` 将成员移出轮转，连续成功达到 `healthy_threshold` 恢复。成员初始为健康；探测用 upstream 的 `host` 作 `Host`/SNI，不带凭据、不经过插件链/限流/token budget、不进请求日志与请求指标。探测结果在 `gateway_upstream_probe_total{upstream, endpoint, result}`（result ∈ ok/connect/timeout/5xx/status_mismatch）中可见；状态变化有独立 warn/info 日志，与被动剔除的 `gateway_upstream_ejected_total` 互不混淆 |
| 成员数上限 | `schema_bounds.max_endpoints`（默认 64），只约束单个 upstream |

**策略**（`strategy`）：

| 值 | 语义 |
|---|---|
| `round_robin` | 按文档顺序轮转（默认）；**不读 `weight`**，写了权重也不改变选择 |
| `weighted_round_robin` | 按 `weight` 比例的平滑加权轮转（`3:1` 交错成 `a,b,a,a`，不是先发三次再发一次）；这是唯一消费 `weight` 的策略 |
| `least_conn` | 当前活跃**请求**数最少者（不是连接池里的 socket 数），平票按轮转；不读 `weight` |
| `ip_hash` | 调用方**完整地址**加 endpoint 名做会合哈希；不读 `weight`，且 endpoint 改名会改变映射。成员增删只迁移约 1/n 的键 |

**失败与剔除**（被动，不做主动探测）：

- 只计三类失败：连接/握手失败、连接超时（含 TLS 握手超时）、上游 5xx。4xx 不计（调用方的问题），
  但它和任何其他应答一样清零连续失败数：链路是通的。
- 连续失败达 `max_fails` 即剔除，冷却 `fail_timeout_ms`。冷却结束后只放**一个**试探请求：
  成功则清零并完整回归，失败则重新计时。
- 冷却不是硬锁：剔除前已被放行的请求如果随后返回应答（含 4xx），成员立即完整回归——证据优先于
  计时器。只有“没有任何新证据”时才等冷却期满。
- **全部被剔时直接拒绝**：整组没有可用成员时回答 `502 upstream_unreachable`，不会从刚被判定
  失败的成员里再挑一个。
- 剔除状态是进程级运行态，与 generation 无关：reload 只换文档。成员身份（upstream 名 +
  endpoint 名 + `address` + `port`）相同的成员继承历史，其余归零。
- 剔除是第三层，与既有两层正交：`retry_budget` 是同一目标的重试，`fallback` 是跨 provider 的链；
  一次请求选定成员后就留在它上面（traffic 路由只有一次尝试），只有 `fallback` 换 provider 时才重新选择。

## 插件

插件是“命名实例”，由路由按名字引用（或列入 `global_plugins`）。声明本身不会让它运行：顶层
`plugins` 只定义实例，`global_plugins` 与 `routes[].plugins` 才决定使用范围与顺序。一个实例不是
kind 别名：两条路由需要不同参数，就写两个同 kind 的实例。

- **`enabled: false` 在全部使用点生效**：声明保留、引用仍合法，但配置不编译、密钥不读取、不运行；
  它不是“只停这条路由”。
- 所有 `enabled` 的定义都会编译，但没有使用点的实例不会运行；反过来，一个写坏的未被引用实例
  仍会让整份文档发布失败。
- 阶段顺序在一个作用域内由插件类别决定：认证排在限流之前；跨作用域则是 `global_plugins` → 路由
  匹配 → 该路由的插件。响应方向反过来。

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

> `buffers_body: true` 会让该路由整体缓冲请求体：请求头照常先发，但上游在客户端最后一个字节
> 到达前收不到 body，因此它的延迟包含整个上传；内存为 `request.max_body_bytes` × 并发缓冲上传数，
> 再加上插件返回的内容。因此它是逐实例 opt-in。插件接口永远不被交给完整响应（响应可能是无限流）；
> 但 LLM convert 路由的非流式应答会由前端整体缓冲，受 `request.max_response_bytes` 约束——两件事
> 不要混为一谈。

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
- 自定义 header（如 `x-upload-key`）按原样匹配，**不带** scheme 前缀；显式写 `header: authorization`
  同样按原样匹配，不再剥 `Bearer`。
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

- 桶按 `(route_id, 实例名, subject|ip)` 分隔。同一实例挂在两条路由上会得到两个桶，额度不会
  跨路由共享；要共享就把实例放进 `global_plugins`（那里 route_id 为空），但全局与路由的两次限流
  是叠加关系，不是覆盖关系。
- 默认按认证插件建立的身份分桶；没有身份时**回退到调用方地址哈希**（不是不限制）。`identity: ip`
  直接按地址哈希分桶，忽略身份。
- 全局作用域在路由之前运行，所以没有 route_id；但它不总是没有身份：同一作用域里排在它前面的
  认证插件已经建立了 subject，全局限流可以按 subject 分桶。
- 桶表是**进程内**的，跨 reload 保留（参数变了重建），多副本之间不共享。
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
    circuit_breaker:
      failure_threshold: 5                   # 连续失败数，0 表示禁用熔断
      cooldown_ms: 30000                     # 打开后等待探针的时间

routes:
  - id: llm-chat
    path_prefix: /v1/chat/
    provider: openai-primary
    protocol: openai.chat     # openai.chat | openai.responses | anthropic.messages | gemini.generate_content
    # timeout_ms 只能收紧 `request.timeout_ms`；写得更大会被截到文档值，不会拒绝发布。
    # 需要两分钟预算时，必须同时调高 `request.timeout_ms`——那会放宽所有没写自己预算的路由。
    max_response_bytes: 4194304   # 只能收紧 request.max_response_bytes
    # retry_budget: 2              # 可选，默认 1（不重试），上限 4
    # fallback: [openai-secondary] # 可选，按顺序尝试，最多 3 个

  # 池形态：一条路由分发到多家 provider，与单个 `provider` 互斥。
  - id: llm-chat-pool
    path_prefix: /v1/chat/
    protocol: openai.chat
    strategy: weighted_random     # weighted_random（默认，按权重抽样）| round_robin | ip_hash（调用方粘性）
    providers:
      - name: openai-primary
        weight: 3                 # 可选，默认 1，0..=1000；0 = 不参与分发，只在兜底显式指名时使用
      - name: openai-secondary
    fallback: [anthropic-primary] # 池耗尽后按顺序尝试
```

### 本地模型服务器

本地推理服务器（Ollama、vLLM、llama.cpp server、TGI 等）**不需要新的 provider kind**：它们多数
提供 OpenAI-compatible 的 `/v1/chat/completions`，用 `kind: openai` 指向本地地址即可。这是
`11-13-heterogeneous-provider-research` 的结论——「异构 provider」里这一类其实只是配置。

```yaml
providers:
  - name: local-llm
    kind: openai
    base_url: http://127.0.0.1:11434/v1   # base_url 可以带路径
    credential_ref: env:NICO_LOCAL_UNUSED # 必填，但本地没有 key：用占位 env 引用
    model_map:
      fast: llama3.2:1b                   # 调用方的逻辑名 → 本地模型名
routes:
  - id: local-chat
    path_prefix: /v1/chat/
    provider: local-llm
    protocol: openai.chat
```

两处说明：

- `credential_ref` 在 schema 里是**必填**且必须通过 `env:` / `file:` 校验。本地服务器不需要密钥，
  写一个占位引用即可（环境变量定义成空值也行）。删掉这个字段会导致整份文档被拒。
- 上述服务器是否真的提供字节兼容的 OpenAI 流式 SSE，是**部署者需要自己核实的外部事实**；
  网关只保证文档形状可发布。完整调研与三类「异构」的边界见
  `.trellis/tasks/archive/2026-11/11-13-heterogeneous-provider-research/research/findings.md`。

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

### token 限额

convert 路由可以按**身份**限制 token 消耗：超限时**下一个**请求得到
`429 token_budget_exceeded`，provider 不再被联系。

```yaml
routes:
  - id: llm-chat
    protocol: openai.chat
    provider: openai-primary
    token_budget:
      limit_tokens: 1000000   # 1..=1_000_000_000
      window_seconds: 86400   # 1..=31_536_000（一年）
```

- **键是身份**：auth 插件（jwt / api_key）建立的身份决定谁的窗口。没有身份的请求**不受限**——
  要全覆盖就在路由上放一个 auth 插件。
- **只计 input + output**：两个全价桶；cache 读写是折扣桶，reasoning 已含在 output 里。
- **事后记账**：用量在响应后才知，一次请求可以越过预算（它的费用已实际发生），拒绝的是
  下一个请求。所有 LLM 网关都这么做，精确预判需要 tokenizer。
- 窗口是固定窗口：首次记账开窗，`window_seconds` 后重置。
- **预算表默认是进程内的**：多副本部署在 bootstrap 写 `quota_store` 段选择共享存储（`etcd` /
  `redis`），每次检查与记账都是后端原子事务；`on_unavailable` 是必填策略——`fail-closed` 在
  存储故障时返回 `503 quota_store_unavailable`（预算不可确认即不放行），`fail-open` 保持请求
  可用但接受故障期间的超额；两种策略的放行/拒绝都在 `quota_store_degraded_total{policy}`
  指标中可见。

### provider 池

`providers` 让一条路由在多条 provider 之间分发，与 `provider` 互斥（都写或都不写都被拒绝）。

| 策略 | 行为 |
|---|---|
| `weighted_random`（默认） | 按 `weight` 比例抽取；成员增减不漂移 |
| `round_robin` | 按池内顺序轮转 |

失败按类别处置，**分类先于任何计数**：

| 失败 | 消耗 `retry_budget` | 成员处置 |
|---|---|---|
| 429 | 否 | 立刻换成员；短冷却 10s |
| 401 / 403 | 否 | 立刻换成员；长冷却 300s |
| 5xx / 连接失败 | 是 | 换成员；熔断器照常计数 |
| 400 / 404 / 422 | — | **不换成员**，直接作答（调用方的错） |

`routes[].ttft_timeout_ms`（`1..=300_000`）给一次尝试的单次读设上限，超时视为该成员失败并切下一个。
**注意语义**：Pingora 的 `read_timeout` 作用于每一次读，所以这个字段实际管的是"一次读最多等多久"，
流式与非流式都适用；一个比预算更慢的缓冲请求也会被它截断。

- **同一请求内不重复选中**：失败过的成员从该请求的候选集里移除。
- **排除表是进程级的**：一次请求发现 429/401 之后，后续请求不再抽到该成员，直到其冷却结束。
- **排除表与熔断器并列不合并**：429/401 只影响"这个成员此刻该不该分流量"，不报告熔断器——否则
  一次限流会打开 300s 的熔断。
- 池上限由 `schema_bounds.max_pool_members` 约束（默认 8），它同时是尝试上界
  `MAX_ATTEMPTS_PER_ROUTE` 的一个因子。
- **硬限制**：请求体超过 64 KiB（Pingora 的重放缓冲区）时，已派发的请求无法重试，这类失败直接
  应答；连接阶段的失败不受此限。

### token 用量

成功转换的请求会记录 provider **自报**的 token 数，按 `(route_id, provider, model, kind)` 暴露为
`gateway_llm_tokens_total`，`kind` ∈ `input` / `output` / `cache_read` / `cache_write` / `reasoning`。

- 只记 provider 报的数。网关没有 tokenizer，也不自己数——自建计数与账单口径必然不同。
- provider 没报的 kind **不产生样本**（不是发 0）：零样本与零值是不同的两件事。
- 失败的响应没有 usage，不记录。
- `model` 标签是 **provider 实际收到的物理名**（`model_map` 的值）。**注意基数**：provider 没有
  `model_map` 时，调用方写的模型名会成为标签，这是唯一一个基数可能随流量增长的地方。

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
| 502 | `upstream_unreachable` | 上游不可达，或该 upstream 没有可用成员（全部被剔除） |
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

`SIGINT` / `SIGTERM` 触发优雅停机：先等 `runtime.grace_period_ms`（默认 5 s）让在途请求跑完，再在
`runtime.graceful_shutdown_ms`（默认 30 s）内 drain。整个进程只有一个 shutdown 源（`CancellationToken`），
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

进程有一个**默认关闭**的自报面，在 bootstrap 的 `metrics.listen` 上独立监听，并可选在那个地址上
终止 TLS（`metrics.tls`）。它不在数据面端口上，所以配置文档里的任何路由都无法遮蔽它。

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

`/metrics` 输出以下指标族：

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
| `gateway_upstream_selected_total` | counter | `upstream`, `endpoint` |
| `gateway_upstream_ejected_total` | counter | `upstream`, `endpoint` |
| `gateway_llm_tokens_total` | counter | `route_id`, `provider`, `model`, `kind` |
| `quota_store_degraded_total` | counter | `policy` |
| `gateway_upstream_probe_total` | counter | `upstream`, `endpoint`, `result` |

两个 `gateway_upstream_*` 的标签取配置里的名字（upstream 名与 endpoint 名），所以基数随文档而定，
不随请求增长。`gateway_upstream_ejected_total` 计的是**剔除事件**，不是状态：冷却后试探失败的成员
不会被再计一次（它从没回来过）。

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
| `upstream.endpoint` | 选中的成员名（仅当该 upstream 有多个成员；单成员部署的日志形状不变） |
| `config.generation` | 当前配置的 generation |
| `http.method` / `http.status` | 请求方法与状态（取消记为 `499`，仅日志） |
| `body_bytes` / `elapsed_ms` | 请求体字节数与耗时 |
| `error.category` / `error` | 失败分类与原因（仅失败时） |
| `authn` | 身份来源（仅认证后） |
| `plugin` | 失败归因的插件实例（仅策略失败时） |
| `llm.protocol` / `llm.provider` / `llm.model` / `llm.stream` | LLM 路由事实（仅 LLM 路由） |

日志级别语义：正常请求 `debug`，调用方可以修正的拒绝 `warn`，需要运维介入的失败 `error`，取消
`debug`。请求路径禁止 `info`。

剔除与回归各一条：成员被剔除时 `warn`（字段 `upstream`、`upstream.endpoint`、`reason` =
`connect` / `timeout` / `5xx`），回归时 `info`。冷却到期但试探仍失败时再记一条 `warn`（同一个成员、
新的失败原因），因为运维刚修好一台机器时最需要看到的就是“试探没通过”。

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
crates/nico-lb/      成员选择契约：Picker trait 与四种策略（不依赖任何传输层）
crates/nico-llm/     LLM 协议编解码、provider 适配器、重试/fallback 链、provider 池
crates/nico-metrics/ 自报面能力层：recorder、Prometheus 渲染、就绪投影（无 socket、无框架）
crates/nico-plugins/ 内置策略：jwt、api_key、rate_limit
crates/nico-server/  启动、reload 管道、进程装配、CLI（二进制 nico）
config/              本地运行的 bootstrap.yaml + resources.yaml
scripts/gates/       git hook 背后的检查
```

## 当前限制

- **配置源**：`type: local` 与 `type: etcd`（`etcd` feature，默认关）都可用。etcd 源是一个 key
  一份完整文档（inclusive 起点、compaction/fragment 折叠 resync、auth 失败与 key 删除都是
  Unavailable 而非空配置）；真实 client 用 `etcd-client` crate 实现，在进程内 etcd v3 mock
  server（官方 rpc.proto 协议替身）上端到端验证，TLS 连接与用户名/密码认证也在覆盖范围内。
  etcd 已支持 `tls`（含可选客户端证书）与 `auth`（用户名 + `env:`/`file:` 口令引用），见上面的
  `config_source：etcd` 一节；仍**未**支持的是：平台证书库回退、逐端点的证书校验名、token/OIDC
  等用户名密码之外的鉴权、以及配置文件热加载。tonic/hyper 栈由 feature 门控：产品二进制（默认与
  `traffic` 角色）不链接，测试构建经 dev-dependencies 使用协议替身。
- **路由匹配**：线性扫描，无编译索引；`path_prefix` 是字节前缀，不做路径归一化。upstream 路由可用
  `rewrite_prefix`（字面前缀交换，query 逐字节保留）与 `retry_budget`（同请求 failover，逐成员健康，
  5xx 透传不重试）；`http.trusted_proxies` 声明受信代理后，限流与 ip_hash 按推导的真实客户端地址键控。
- **上游成员解析**：endpoint 的 `address` 写域名时，发布期解析一次并展开为逐地址成员（逐地址健康与
  故障转移）；解析结果不做 TTL 后台重解析，DNS 变更随下一次 reload 生效。LLM provider 的 `base_url`
  不展开——provider 目标仍是每请求首个解析地址。
- **入站 TLS**：`http.listeners[]` 中带 `tls` 块的条目终止 TLS——单证书、启动期加载（TLS 1.2/1.3，
  ALPN h2+h1），证书/私钥变更需重启；明文条目与 TLS 条目可以在同一份配置里并存（即 nginx
  `listen 443 ssl` 的形状）。证书热加载/轮换、mTLS、SNI 多证书、同一端口上自动识别明文与 TLS
  暂未实现；需要这些能力时仍在前面放代理。上游方向直接支持 `https`（见 `proxy.tls.ca_file`）。

从 nginx 迁移的逐项对照、会改变请求行为的写法和检查清单见
[`docs/nginx-migration.md`](docs/nginx-migration.md)。
