# 同一插件、不同路由参数：七个网关的配置绑定模型

> 调研对象：Envoy / Kong / APISIX / Traefik / Higress+Istio / nginx / Bifrost（+ aiway 补充）。
> 结论只引用源码或官方文档；无法核对的条目写"未查到"或"未核实"，见 §6。
> 本文件只做调研，不改代码。环境：`/Users/tomgs/workspaces/go_space/bifrost`、`/Users/tomgs/workspaces/rust_space/aiway`
> 为本地检出；其余为 2025-11 抓取的 `raw.githubusercontent.com` / 官方文档。

---

## 1. 一句话结论

**没有一家主流网关把"路由内联任意配置"当默认**：Kong / APISIX 走"按作用域层级各挂一份完整配置"（同名 kind
每个作用域一个独立实例），Envoy / nginx 走"共享声明 + 路由级小旋钮/选择器"。真正支持"同一插件按路由拿不同
任意配置"的只有 Envoy 的 `filter_chain` 与 Higress 的 `match_rules`，而它们**不共享编译产物**——每个匹配各自
一份配置。nico 的"定义一次 + 按名字引用"是其中的 B 模型，对 jwt/api_key/rate_limit 三个内置 kind 是合适的；
升级到 C（共享 provider + 路由级小旋钮）有明确触发条件，见 §5。

---

## 2. 对照表

| 网关 | 配置放在哪 | 同一 kind 多实例 | per-route 能改什么 | 共享状态按什么键 | 配置变更原子性 |
|---|---|---|---|---|---|
| **Envoy**（`typed_per_filter_config`） | HCM `http_filters` 里的**命名 filter 条目** + `RouteConfiguration` / `VirtualHost` / `Route` / `ClusterWeight` 四级 per-filter 覆盖 | 是：同一 chain 内两个条目用不同 `name` | **由 filter 自己决定**：`jwt_authn` 只能 `disabled` / `requirement_name`；`ext_authz` 只能 `disabled` / `CheckSettings`（小旋钮）；`local_ratelimit` 可整份 `token_bucket` | `local_ratelimit`：每个 route 的 `FilterConfig` 自带桶，"bucket becomes unique to that route"；`jwt_authn`：provider 级（filter 级共享 JWKS） | RDS 返回**整份** `RouteConfiguration`，一次替换 |
| **Envoy**（`filter_chain` filter） | 路由内的完整 filter 链（任意 filter + 任意 config） | 是，纯内联 | 任意 | 无共享（每条路由的 filter 各自实例） | 同上 |
| **Kong** | 独立的 `plugins` 实体行，挂 global / service / route / consumer / consumer-group | 是：每个作用域组合一行；**同一请求只跑最具体的一行** | 整份 `config`（另建一个实体） | 插件实体自身（+ 插件的 shared dict / Redis，key 含实例） | decK 整份 `deck sync` diff；DB 模式逐实体写 + 缓存事件 |
| **APISIX** | route / service / consumer / consumer_group / global_rule / `plugin_config` 上的 `plugins: {name: config}` 映射 | 是：不同对象各一份；同名在一条请求内**只跑一份**（global_rule 另算，顺序执行） | 整份 `config`（内层同名覆盖外层） | `parent.resource_key : crc32(conf) : key` | etcd 逐 key watch，资源各自版本（`modifiedIndex`） |
| **Traefik** | 命名 `Middleware` 对象；router 用 `middlewares: [name]` 引用 | 是：多个命名对象 | **只能换名字**（router 侧只有名字列表，无覆盖字段） | 按 middleware 实例 | provider 全量下发（file / k8s / label） |
| **Higress** | 一个 `WasmPlugin` + `match_rules[]`，每条规则一份 `config` | 是：同一插件资源内多份 config | 按 `ingress` / `domain` / `service` / `route_type` 选一份 config，或 `config_disable` | 按 wasm 插件实例 + matchRule | CRD 逐资源 reconcile |
| **Istio 原生** | `WasmPlugin` 单份 `pluginConfig`；`match` 只选**工作负载**（mode+ports） | 需要多个 `WasmPlugin` 资源 | **无 per-route**（`TrafficSelector` 里没有路由维度） | 按插件实例 | CRD reconcile |
| **nginx** | 声明 `limit_req_zone`（http 级，定义 key+rate+zone） + 使用点 `limit_req zone=... burst=... nodelay` | 是：多个 zone | `zone` 选择 + `burst` / `nodelay` / `delay` 三个旋钮（rate 在 zone 里，改不了） | zone（共享内存），key 在 zone 声明里 | reload 整份配置 |
| **Bifrost** | **进程级** `plugins[]`，每条一份 config，一条一个实例；没有路由实体挂插件 | 是（多条 entry） | **无 per-route**（策略在插件自己的 store/DB 里） | 插件内部 | 配置文件 / DB 重载 |
| **aiway**（补充） | 使用点内联 `{name, config}`：全局一份列表 + 每条路由一份列表；`name` 只解析到 wasm 模块 | 是（同名多份 config 共享**同一个**模块实例） | 任意（config 每次调用注入） | 按 `name`（模块实例） | 拉全量 + hash 对比后整体替换 |

---

## 3. 逐个网关

### 3.1 Envoy：per-filter config 是"filter 自己定义的残缺覆盖"

**机制。** 四层都可以挂 `typed_per_filter_config`，key 是 HCM `http_filters` 条目的 `name`（不是 type URL）：

- `RouteConfiguration.typed_per_filter_config` — `api/envoy/config/route/v3/route.proto:161`
- `VirtualHost.typed_per_filter_config` — `api/envoy/config/route/v3/route_components.proto:167`
- `Route.typed_per_filter_config` — 同文件 `:364`
- `WeightedCluster.ClusterWeight.typed_per_filter_config` — 同文件 `:568`

```protobuf
// route_components.proto:358-364
// This field can be used to provide route specific per filter config. The key should match the
// filter config name <...HttpFilter.name>.
map<string, google.protobuf.Any> typed_per_filter_config = 13;
```

**解析。** 值是一个 `Any`，在**路由配置加载期**被解析成强类型对象，不是请求期才反序列化：

```cpp
// source/common/router/per_filter_config.cc:43-60
factory = getFactoryByType<NamedHttpFilterConfigFactory>(typed_config);
if (factory == nullptr) {
  if (is_optional) { ENVOY_LOG(warn, ...); return nullptr; }
  return absl::InvalidArgumentError(fmt::format(
      "Didn't find a registered implementation for '{}' with type URL: '{}'", ...));
}
proto_config = factory->createEmptyRouteConfigProto();
translateOpaqueConfig(typed_config, validator, *proto_config);
factory->createHttpFilterRouteConfig(*proto_config, factory_context, extra_context);
```

工厂侧默认**不实现** per-route（`envoy/server/filter_config.h:414-419`，`createRouteSpecificFilterConfig`
默认 `return nullptr;`）；此时若路由写了 per-route config 且未标 `is_optional`，加载期就报错
`"The filter {} doesn't support virtual host or route specific configurations"`（`per_filter_config.cc:56-60`）。
**这是"未实现的钩子 = 配置期拒绝"，不是运行期忽略。**

返回值必须派生自 `Router::RouteSpecificFilterConfig`（`envoy/router/router.h:141-150`），并存进已加载的路由表。

**覆盖语义有两种，filter 按需选**：

```cpp
// envoy/router/router.h:438-448
// helper to get the route's per-filter config if it exists, up along the config hierarchy
// (Route --> VirtualHost --> RouteConfiguration). Or nullptr if none of them exist.
virtual const RouteSpecificFilterConfig* mostSpecificPerFilterConfig(absl::string_view name) const PURE;
// Return all the available per route filter configs. The configs is in order of specificity.
// ... route configuration first, then virtual host, then route.
virtual Router::RouteSpecificFilterConfigs perFilterConfigs(absl::string_view name) const PURE;
```

- `mostSpecificPerFilterConfig` = **整体覆盖**：最近的层级赢，不合并字段（`config_impl.cc:1802-1806`、
  `:1416-1421`）。
- `perFilterConfigs` = **逐层合并**，给需要字段级叠加的 filter 用（`config_impl.cc:1808-1825`）。

**三个关键判断。**

1. **"任意覆盖"还是"共享 provider + 路由级开关"？——两种都有，逐 filter 不同。** Envoy 把选择权交给
   filter，所以答案要在每个 filter 的 per-route proto 里读：

   ```protobuf
   // .../jwt_authn/v3/config.proto:948-961
   message PerRouteConfig {
     oneof requirement_specifier {
       bool disabled = 1;                 // 本路由关掉 JWT
       string requirement_name = 2;       // 必须是 filter 级 requirement_map 里的键
     }
   }
   ```
   `requirement_name` 的注释（`:955-959`）明确："This requirement_name MUST be specified at the
   `requirement_map` … If no, the requests using this route will be rejected with 403."
   → **jwt_authn 是纯 C 模型**：provider 只在 filter 级定义一次，路由只能选一个名字或关掉。

   ```protobuf
   // .../ext_authz/v3/ext_authz.proto:633-649
   message ExtAuthzPerRoute {
     oneof override {
       bool disabled = 1;
       CheckSettings check_settings = 2;
     }
   }
   ```
   → **ext_authz 也是 C**：不能换 auth server，只能关掉或调 `context_extensions`（`:663-671`："Merge
   semantics … keys from more specific configs override"）、`disable_request_body_buffering` 等小旋钮。

   ```protobuf
   // .../local_ratelimit/v3/local_rate_limit.proto:44-56
   // It's fine for the token bucket to be unset for the global configuration …
   // Thus, the token bucket must be set for the per route configuration otherwise the config will be rejected.
   // When using per route configuration, the bucket becomes unique to that route.
   type.v3.TokenBucket token_bucket = 3;
   ```
   → **local_ratelimit 是"整份 per-route 配置"**（A/D 味道）：每个路由可以有自己的 `max_tokens`/`fill_interval`，
   且**状态天然按路由分开**。加载期强制（`local_ratelimit.cc:84-89`：`"local rate limit token bucket must be
   set for per filter configs"`）；请求期 `decodeHeaders` 用
   `Http::Utility::resolveMostSpecificPerFilterConfig<FilterConfig>(decoder_callbacks_)` 覆盖
   `used_config_`（`local_ratelimit.cc:171-176`）。外部 ratelimit filter 的 `RateLimitPerRoute` 则是中间态
   （`rate_limit.proto:205-264`：`vh_rate_limits` 三态、`rate_limits` 描述符、`domain` 覆盖；`stage` 与
   `limit` 在 per-route 里**不支持**）。
2. **状态键。** 取决于 filter：`local_ratelimit` 的桶在 `FilterConfig` 里（per-route 就是 per-route 一个桶）；
   `jwt_authn` 的 JWKS/provider 在 filter 级（所有路由共享），per-route 只选名字。**"共享 provider" 换来的
   正是"JWKS 只缓存一份、只刷新一次"**——这是 C 模型真正的收益。
3. **两条附加能力。** ① `HttpFilter.disabled`（`hcm.proto:1436-1442`）+ `FilterConfig.disabled`
   （`route_components.proto:3085-3098`）= filter 默认不在链里，由路由**显式启用**；② `FilterConfig`
   还可以包 `{config, is_optional}`（`per_filter_config.cc:96-139`）。

**"每条路由要完全不同的配置"的官方答案：`filter_chain` filter。**

```protobuf
// .../filters/http/filter_chain/v3/filter_chain.proto:28-40
// When a request arrives the filter collects all active filter chains in order from least
// to most specific: default_filter_chain first, then any per-route chains ordered from
// the outermost scope … Each filter in a less-specific chain is applied **unless** a
// more-specific chain contains a filter with the same name.
```

即：路由可以带一条**完整的** chain，任意 filter、任意 config；同 `name` 以更具体者为准。这就是 D 模型，代价是
配置量爆炸 + 每个匹配各自实例（无编译产物复用）。

**版本/来源**：Envoy `main` 分支，2025-11 抓取。

---

### 3.2 Kong：插件是一等实体，一行 = 一个实例 = 一份 config

**机制。** `plugins` 是一张表，`name` 是 kind（subschema），作用域是三个可空外键：

```lua
-- kong/db/schema/entities/plugins.lua:8-30（节选）
cache_key = { "name", "route", "service", "consumer" },
subschema_key = "name",
subschema_error = "plugin '%s' not enabled; add it to the 'plugins' configuration property",
fields = {
  { name = { ... required = true }, },
  { instance_name = typedefs.utf8_name },      -- v3.15+，纯标签
  { route    = { type = "foreign", reference = "routes",   on_delete = "cascade" }, },
  { service  = { type = "foreign", reference = "services", on_delete = "cascade" }, },
  { consumer = { type = "foreign", reference = "consumers", on_delete = "cascade" }, },
  { config   = { type = "record", abstract = true }, },   -- 由 kind 的 subschema 定义
  { enabled  = { type = "boolean", required = true, default = true }, },
}
```

**语义（官方文档原文）。**

- "A plugin can have multiple instances in the same configuration. Different instances can be used to apply the
  plugin to various entities, combinations of entities, or even globally. You can give each plugin instance a
  unique name … **The instance name itself doesn't affect processing - it acts like an internal label instead.**
  **A single plugin instance always runs once per request.**"（`developer.konghq.com/gateway/entities/plugin/`）
- 优先级：参与的实体越多越具体，`Consumer+Route+Service` 最高，`Route` 次之，**Globally** 最低；即
  **最具体的一行独占**（不叠加）。
- 要在**同一条请求上跑同一 kind 的两个实例**（例：校验两个 JWT），v3.15+ 需要 **clone**：克隆出一个独立的
  plugin 名（"shares its code and logic … treated as a separate plugin instance"），且只对白名单插件开放
  （ACL / Key Auth / Request Transformer / Rate Limiting Advanced 等）。

**关键判断。** Kong = **A 模型的层级化版本**：每个使用点是一行完整 config。"同一 kind 不同路由不同参数"是
**一等公民**，代价是参数相同也要复制整份 config（改一处公共参数要改 N 行）。`instance_name` 只做可观测标签，不
做参数共享——Kong 没有 C 模型。

**声明式（decK）里就是同一张表**：`_format_version: "3.0"` + 顶层 `plugins:` 列表，每项 `name` + `config` +
可选作用域键（文档中的 Admin API 形态为 `POST /routes/{id}/plugins` 或 `plugin.route` 字段）。

---

### 3.3 APISIX：`plugins` 是 name→config 映射，内层同名整体覆盖

**机制。** 五个对象上都有 `plugins` 字段（`apisix/schema_def.lua`）：route `:687`、service `:773`、
consumer `:808`、global_rule `:1030`、consumer_group `:1189`，外加 **`plugin_config`（`:1156-1175`）**——一个只有
`id`+`plugins` 的可复用**命名包**，路由/服务通过 `plugin_config_id` 引用（`schema_def.lua:688`，
`init.lua:1018-1027`）。

覆盖规则是**按插件名整体替换**，不是字段合并：

```lua
-- apisix/plugin.lua:705-730（merge_service_route，节选）
local new_conf = core.table.deepcopy(service_conf)
if route_conf.value.plugins then
  for name, conf in pairs(route_conf.value.plugins) do
    new_conf.value.plugins[name] = conf     -- 同名整体覆盖
  end
end
```

`plugin_config` 合并方向相反——**路由自己的内联 plugin 优先**，命名包只补空缺（`apisix/plugin_config.lua:86-90`），
且 `plugin_config_id` 取不到时**直接 503**（`init.lua:1019-1024`，fail-closed）。

一条请求内同一插件只跑一份，优先级：

```
Consumer > Consumer Group > Route > Plugin Config > Service      -- docs/en/latest/terminology/plugin.md:127-135
```

global_rule 例外：与对象级实例**顺序执行两次**（同上，`:124-126`）。每份配置上的通用旋钮放在 `_meta` 里
（`disable` / `error_response` / `priority` / `filter`，同文件 `:137-147`）——**per-instance 开关是配置的一部分，
不是信封的一部分**。

**状态键（对 nico 最有参考价值的一点）。**

```lua
-- apisix/plugins/limit-req.lua:128-136
local function gen_limit_key(conf, ctx, key)
  local parent = conf._meta and conf._meta.parent
  return parent.resource_key .. ':' .. apisix_plugin.conf_version(conf) .. ':' .. key
end
-- plugin.lua:1465-1478：parent.resource_key = 该作用域资源的 etcd key
-- plugin.lua:999-1008：conf_version = crc32(stably_encode(conf))
```

即桶命名空间 = **作用域资源 key × 插件配置内容哈希 × 用户 key**。改配置 ⇒ 计数器自动归零重开，不会继承旧
参数的桶。`limit-req` 的 `key`/`key_type`（`limit-req.lua:47-53`，`var` / `var_combination`）是**使用点**的字段，
不是独立 provider；`policy: local|redis|redis-cluster` 决定桶在本地还是外部。

`plugin_metadata` 是 APISIX 的另一条缝（插件级、跨路由的元配置）：本次**未查到**一手文档页（`terminology/plugin.md`
未收录），故不在表中给结论。

---

### 3.4 Traefik：命名对象 + 纯引用，参数不可能 per-router

```yaml
# docs/content/middlewares/overview.md（v3.3）
http:
  routers:
    router1:
      rule: "Host(`example.com`)"
      service: myService
      middlewares: ["foo-add-prefix"]      # 只有名字，没有参数
  middlewares:
    foo-add-prefix:
      addPrefix: { prefix: /foo }
```

router 侧只有一个名字列表（`docs/content/routing/routers/index.md:587-608`："You can attach a list of
middlewares to each HTTP router"），名字还可带 provider 命名空间（`foo-add-prefix@file`）。**结论：同一 middleware
被两个 router 引用 = 完全相同的参数**（router 没有覆盖字段）。业界的绕法就是 Traefik 自己的做法：**定义两个
命名对象**——即 B 模型的"复制配置"代价，Kubernetes 里通常用 Helm/Kustomize 模板生成 N 个 Middleware CRD。

---

### 3.5 Higress / Istio：Istio 只到工作负载，Higress 加了路由维度

```protobuf
// istio/api/extensions/v1alpha1/wasm.proto:332 / :355-395
google.protobuf.Struct plugin_config = 7;      // 一个 WasmPlugin 只有一份 config
message TrafficSelector {                      // match 只按 mode + ports
  istio.type.v1beta1.WorkloadMode mode = 1;
  repeated istio.type.v1beta1.PortSelector ports = 2;
}
repeated TrafficSelector match = 12;
```

→ Istio 原生**没有**路由级插件配置：一个 `WasmPlugin` 一份 config，`match` 选的是**工作负载**（pod/端口）。
同一工作负载要多份不同 config，只能建多个 `WasmPlugin`（本次未核实其多重匹配的合并/冲突规则，见 §6）。

Higress 的扩展正是把"按路由选择"补上：

```protobuf
// alibaba/higress api/extensions/v1alpha1/wasmplugin.proto:84-127
google.protobuf.Struct plugin_config = 7;
google.protobuf.Struct default_config = 101;             // Higress 扩展：全局默认
repeated MatchRule match_rules = 102;                    // Higress 扩展：按路由生效
google.protobuf.BoolValue default_config_disable = 103;
message MatchRule {
  repeated string ingress = 1;
  repeated string domain = 2;
  google.protobuf.Struct config = 3;                     // 该匹配下的一整份 config
  google.protobuf.BoolValue config_disable = 4;
  repeated string service = 5;
  RouteType route_type = 6;
}
```

→ **D 模型**：一个插件资源内多份 config，按 (ingress, domain, service) 选择；`default_config` + `config_disable`
提供默认与逐匹配关闭。

与 Envoy `typed_per_filter_config` 的关系：Envoy 侧一个 wasm 插件在 chain 中只有一个 filter 条目，因此"同插件
多份 config"在 Envoy 里只能表达为该 filter 的 per-route config。Higress 的 `match_rules → per-route wasm config`
这条翻译在其控制面代码里，本次**未定位到实现文件**（`pkg/ingress/kube/wasmplugin/controller.go` 只有 CRD
watch，未见 `typed_per_filter_config`），故标为**推断，未核实**。

---

### 3.6 nginx / OpenResty：声明与使用点拆开，使用点只留三个旋钮

```
limit_req_zone $binary_remote_addr zone=perip:10m rate=1r/s;     # http 级：key + rate + 共享内存
limit_req_zone $server_name        zone=perserver:10m rate=10r/s;

server {
  limit_req zone=perip     burst=5 nodelay;                     # 使用点：选 zone + burst/nodelay/delay
  limit_req zone=perserver burst=10;
}
```

- `limit_req_zone` 只在 `http` 上下文；`limit_req` 在 `http|server|location`；**rate 属于 zone 声明**，使用点
  改不了（`nginx.org/en/docs/http/ngx_http_limit_req_module.html`）。
- 继承规则："These directives are inherited from the previous configuration level **if and only if** there are no
  `limit_req` directives defined on the current level."（同上）→ 内层出现即**整体替换**，不合并。
- 多个 `limit_req` 可**同时生效**（上例 per-IP + per-server 两条）。
- 认证侧是 per-location 全量配置：`auth_jwt` / `auth_jwt_key_file` / `auth_jwt_key_cache` 都在
  `http|server|location`（`nginx.org/en/docs/http/ngx_http_auth_jwt_module.html`，**商业订阅**模块）；
  OpenResty 则是每个 location 一段 `access_by_lua_*` 脚本。

**这是 C 模型最干净的范本**：**"量的定义"（zone: key + rate + 内存）与"用的选择"（zone + burst）分离**；代价是
一个 zone 一套 rate——两个 location 想用同一 key 但不同 rate，必须声明两个 zone（本次读文档得的直接推论，未实测）。

---

### 3.7 Bifrost（本地）：插件只在进程级，路由差异写在插件自己的库里

```go
// framework/plugins/main.go:10-25
type PluginConfig struct {
    Path    string `json:"path"`
    Name    string `json:"name"`
    Enabled bool   `json:"enabled"`
    Config  any    `json:"config,omitempty"`
}
type Config struct {
    Plugins []PluginConfig `json:"plugins"`
}
// :104-120 LoadPlugins：遍历 entries，`LoadPlugin(pc.Path, pc.Config)`——每个 entry 一个新实例；
//                    且 **`pc.Name` 未参与加载**（实例身份来自模块自身 GetName()）。
```

- 载体只有一处：`transports/bifrost-http/lib/config.go:234` 的顶层 `plugins []*schemas.PluginConfig`；
  `core/schemas/plugin.go:521-540` 的 `PluginConfig` 再加 `Placement`/`Order`/`SemaphoreSize`/`InjectTimeout`
  这类**信封级**旋钮。
- 没有任何路由/服务实体携带插件列表：`core/schemas/bifrost.go:31-34` 只有 `LLMPlugins`/`MCPPlugins`/…（按接口
  能力筛出的切片），不是按路由的绑定。
- 路由差异在哪？**在插件自己的配置与状态里**：`plugins/governance` 的 `Config` 只有 `is_vk_mandatory` /
  `required_headers` 等，真正的策略在它的 store（DB）；`plugins/routing` 的 `Config` 是 `routing_chain_max_depth`
  + `complexity_analyzer_config`，规则在 `rules.Store`；插件还能在请求期改路由（
  `core/schemas/bifrost.go:402` 的 `BifrostContextKeyRoutingAllowedProviders`）。
- 即 Bifrost 是"**A 模型 + 进程级作用域 + 策略外置到插件自己的 DB**"，第五种形态；它不需要 per-route 绑定，因为
  它的"路由"是模型/供应商选择，由插件在运行期决定。

---

### 3.8 aiway（补充）：使用点内联 config，模块实例按名字共享

这与 nico 的参考模型直接相关，且与设计文档里"插件应在路由与全局之间共享"的直觉相反：

```rust
// lib/aiway-protocol/src/gateway/plugin.rs:6-31
pub struct Plugin { pub name: String, pub url: String, pub version: String, pub checksum: String }  // 模块目录
pub struct ConfiguredPlugin { pub name: String, pub config: serde_json::Value }                     // 使用点
// lib/aiway-protocol/src/context/route.rs:39            pub plugins: Vec<ConfiguredPlugin>,
// lib/aiway-protocol/src/gateway/global_filter.rs:11    pub plugins: Vec<ConfiguredPlugin>,
```

- 模块**按 name 只加载一次**（`lib/plugin-manager/src/plugins.rs:28-30`，`DashMap<String, (PluginConfig, Box<dyn Plugin>)>`）。
- 配置**每次调用注入**：`inject_config(ctx, &configured_plugin.config)`（`:149-154`，写进
  `HttpContext::PLUGIN_CONFIG` 供 host function 读），再调用 `on_request` 等（`:156-210`）。
- 因此同一 name 在两条路由上可以带两份完全不同的 config，而**共享同一个模块实例**——所以协议里写死
  "注意：插件应无状态化"（`plugin.rs:4-5`）。**没有 per-instance 编译产物**：模板/密钥/参数结构都只能在
  `on_*` 里现解析（或者自己按 config 哈希做缓存）。
- 失败路径：插件自身异常一律 502 `BadPlugin`（`gateway/src/handler/plugin.rs:23-33`）——正是 nico 刻意偏离的地方。

aiway 证明："同名多份 config" 与 "编译一次" 不可兼得：它选了前者，代价是每次调用注入 + 插件必须无状态。

---

## 4. 四条模型的代价

| 维度 | **A 使用点内联** | **B 命名对象引用**（nico 现状） | **C 共享 provider + 路由级小旋钮** | **D 一份插件多份 config 按路由匹配** |
|---|---|---|---|---|
| 编译一次 | ✗ 每个使用点一份 config（aiway 共享模块但每次调用注入；Envoy `filter_chain` 每路由实例化） | ✓ 一个实例 `Arc<dyn Plugin>`，多链共享（design §6.2） | ✓ 一个 provider 一份重状态（JWKS/桶表） | ✗ 每个匹配一份 config 对象 |
| 状态键控 | 宿主不掌握"实例"，状态只能靠配置哈希或 scope 区分 | 实例即命名空间；路由信息再从 ctx 取（nico：`hash(route_id, subject/ip)`） | provider 是天然键（Envoy：JWKS 共享；桶按 provider 或 route） | 每个匹配一个状态槽，复用性最差 |
| 可观测身份 | 日志里"哪份配置"要额外造 ID（aiway 只有 name） | **名字就是身份**：`authn`、编译日志、限流键都用它 | 身份 = provider 名 + 路由（两个维度） | 身份 = 匹配规则（难读） |
| 校验时机 | 每次使用点都要校验；漏一处就漏一处 | 发布期一次，错误带实例名（design §6.1） | 声明期校验 provider，使用点只校验开关类型 | 规则越多，校验与报错定位越难 |
| 典型代价 | 配置重复；无预编译 | **参数相同的 N 个实例要复制 N 份 config** | 路由能表达的自由度受 provider 字段限制 | 配置量爆炸；无编译产物复用 |

补充两条表中放不下的：

- **A/B 的真正区别不是"能不能多份配置"，而是"多份配置是否各有独立实例"**。aiway 是"多份 config、单实例"，
  Kong/APISIX 是"多份 config、多实例"。前者省内存但逼插件无状态；后者能做预编译但参数复制重。
- **D 的隐藏成本是顺序**。Envoy `filter_chain` 要靠"同名更具体者覆盖 + 由外到内排序"来定义顺序；nico 现在
  把顺序写死在编译期（认证类先于限流类），一旦允许 per-route 链，就要回答"路由链怎么与全局链交错"，这是
  PRD R2 目前明确拒绝的自由度。

---

## 5. 对 nico 的建议

### 5.1 按名字引用是否合适：**是，保留 B**

四条理由，都能落到现有契约上：

1. **三个内置 kind 的 per-route 差异都不大**。`jwt` 的差异是"用哪个 issuer/audience"，`api_key` 是"用哪个 key
   集合"，`rate_limit` 是 `burst`/`per_second`/`identity`（`crates/nico-plugins/src/rate_limit/config.rs:25-36`）。
   这些差异在真实部署里通常是"每个上游一个"，而不是"每条路由一个"——实例数量与上游数量同阶，配置重复可接受。
2. **B 换来的三件事正好是 PRD 反复强调的**：编译一次（§6.2 intern + 共享 `Arc`）、身份即日志字段
   （R8 的 `authn` = 认证插件实例名）、发布期一次校验（R5「未知字段由工厂拒」）。
3. **B 比 Envoy/Kong 的默认模型更强的一点**：同一路由可以同时挂**两个同 kind 实例**（两个 JWT、两条限流），
   因为这是列表里的两个名字。Envoy 要靠 `filter_chain`，Kong 3.15+ 要靠 clone 白名单，APISIX/Kong 更只能
   "每插件每请求跑一份"。这条能力**现在就有，不要为了 per-route 参数丢掉它**。
4. **状态键已经天然分路由**：`bucket_key = hash(route_id, subject|ip)`（`crates/nico-plugins/src/rate_limit.rs:78-97`），
   共享一个实例的两条路由各自计数——与 Envoy `local_ratelimit` 的"bucket becomes unique to that route" 同义。
   若改成 per-route 内联 config，反而要重新回答"两条路由的桶算不算一个"。

### 5.2 什么时候值得升级到 C：**可判定的触发条件**

满足**任意两条**再动，否则维持 B：

1. **同一个 kind 的实例数 ≥ 5，且 ≥ 3 个参数相同、只有 1–2 个标量不同**（例：5 条路由只差 `burst`）。此时
   "改公共参数要改 5 处"已经是正确性风险，不再是审美问题。
2. **出现"共享重状态"的明确需求**：两个路由要用**同一份** JWKS 缓存 / 同一个配额池，而不仅仅是同样的参数。
   这是 C 的核心收益（Envoy 的 provider 就是为这个存在的）；只共享参数、不共享状态时，B 的复制并不比 C 差。
3. **operator 的实际编辑模式**：如果运维是通过 Helm/Kustomize/生成器产出文档，B 的复制是零成本的，**不要升级**；
   如果是人手改一份 YAML 且已出现"改了 A 忘了 B"的事故，才升级。
4. **顺序需要 per-route 化**（例如某条路由要求限流先于认证）。这会直接冲撞 PRD R2「书写顺序不能反转」，
   属于需求变更而不是配置模型演进——单独开任务。

### 5.3 若升级，选形态而不是选"内联"

不要引入 A（使用点内联完整 config）：它会同时打破 §5.1 的四条理由，并把 `deny_unknown_fields` 的校验点从
"发布期一次"变成"每个使用点一次"。

建议的形态是 **C 的窄版本**，且 selector 归 kind 所有：

```yaml
plugins:                        # 定义：provider 级，重状态与共享参数
  - name: edge-quota
    kind: rate_limit
    config: { per_second: 100, max_keys: 10000, identity: subject }
routes:
  - id: api-v1
    plugins:
      - edge-quota              # 现状：整份继承
      # 将来（若触发 §5.2）：kind 自己的字段，core 只透传
      - { name: edge-quota, use: { burst: 20 } }
```

三个约束，缺一不可：

- `use` 的结构由 **kind 的工厂**解释（与 `config` 同一份 `deny_unknown_fields` 逻辑），`nico-core` 仍然只认信封；
- `use` 只允许**收紧**（与 `routes[].max_body_bytes` 的护栏语义一致），不允许放宽 provider 的 `per_second`；
- 编译期把 `(name, use)` 折叠成一个"有效配置指纹"，仍进现有的 interning/桶表复用（APISIX 的做法：
  bucket key 含 `crc32(conf)`，配置变了桶自动重开，`apisix/plugins/limit-req.lua:128-136`）。
- **不引入"路由级任意覆盖"**：那等于 D，会引出 §3.1 的 `filter_chain` 那套复杂度（链交错、顺序、实例化成本）。

### 5.4 两个已知缺口，建议写进文档而不是改模型

- **per-use 关闭**。B 下"这条路由不用这个全局插件"只能从列表里删名字；APISIX 有 `_meta.disable`、Envoy 有
  `FilterConfig.disabled`、Kong 有 `enabled` 实体。nico 的 `enabled: false` 是**实例级**（全局停用），无法只对
  一条路由关闭。若出现"全局默认开、个别路由关"的运维诉求，最小改动是给使用点加 `{name, disabled: true}`
  （仍是信封字段，仍是 core 语义），而不是引入 per-route config。
- **参数复制**。B 的代价要显式写在 `config/resources.yaml` 注释与 `design.md` 风险表：N 条路由要不同 `burst`
  = N 个实例 = N 份 config；共享实例则桶按路由分开（这是设计意图，不是 bug）。

---

## 6. 未核实清单

| 结论 | 状态 |
|---|---|
| Higress `match_rules` 被翻译成 Envoy `typed_per_filter_config` | **推断，未核实**。`pkg/ingress/kube/wasmplugin/controller.go` 只有 CRD watch，未找到翻译实现文件 |
| Istio 多个 `WasmPlugin` 同时匹配同一工作负载时的合并/冲突规则 | **未查到**（proto 无说明；未查 Istio 文档的 precedence 段） |
| 两个 `limit_req` 使用同一 key 但不同 rate 必须声明两个 zone | **文档推论，未实测** |
| nginx `limit_req_zone` 在 `stream`/`server` 上下文不可用 | 未查（只核对了 `limit_req` 的 context 列表） |
| APISIX `plugin_metadata` 的语义与典型用法（`limit-req` 是否用） | **未查到一手文档**；`limit-req.lua` 中未见读取 `plugin_metadata` |
| Kong 在**同一作用域**能否存在两行同名 plugin（除 clone 外） | 由 `cache_key = {name, route, service, consumer}` 推断为"不能"，未找到明文说明 |
| Envoy 同一 chain 内两个同类型 filter（不同 `name`）的官方示例 | 由 `HttpFilter.name` + `per_filter_config.cc` 的类型 URL 查找推断可行，未找到官方示例 |
| Bifrost 是否有企业版/插件内部的路由级配置（未开源部分） | 未查；仅基于本仓库 `main` 分支 |
| aiway 官方 `rate_limit` 插件如何键控桶（插件在独立仓库 `xgpxg/aiway-plugins`） | 未查；本报告只描述宿主侧模型 |
| 各网关"配置变更原子性"的细节（Kong DB 模式的 cluster event、APISIX etcd 逐 key 生效窗口） | **基于文档与代码结构的概括**，未做时序实测 |
