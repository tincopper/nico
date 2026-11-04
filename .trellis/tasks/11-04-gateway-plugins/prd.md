# 内置 HTTP 策略：JWT / API Key / 限流

## Goal

建立 `gateway-plugins`：提供三个内置 HTTP 策略（JWT、API Key、Rate Limit），以 `gateway_http::HttpFilter` 实现，通过路由配置引用并在 `gateway-server` 注册。策略是受信任的静态组合，不做通用插件容器。

## Dependencies

- 前置：`http-data-plane`（需要 `HttpFilter` trait、请求上下文与响应提交语义）。
- 前置：`route-schema`（需要路由模型；本任务会按需追加策略引用字段）。
- 后续：无强依赖；`gateway-ai` 可复用同一 Filter 机制。

## Requirements

### R1：配置模型（追加）

- 在文档中增加 `policies` 段与路由级 `policies: [name]` 引用（字段名与 `route-schema` 对齐，必要时回写其 PRD/实现）。
- 策略类型：`kind: jwt | api_key | rate_limit`，各自强类型配置，`deny_unknown_fields`。
- 引用校验：路由引用的策略必须存在；同一路由内策略顺序按文档顺序执行，但**认证先于限流**（由注册顺序保证，配置不得反转）。
- secret 只存引用（`env:VAR` / `file:/path`），文档中不得出现明文密钥。

### R2：JWT

- 支持 HS256；校验 `exp`、`nbf`（若存在）、`iss`、`aud`（按配置）。
- 缺失/格式错误 → 401；签名或声明无效 → 401/403（按配置区分），错误码稳定。
- 不在日志、错误响应中出现 token 内容；失败时最多记录 `kid` 或前 4 位。

### R3：API Key

- 从 `Authorization` 或自定义 header 读取；与配置的 key 集合做**常数时间比较**（允许存哈希）。
- 缺失 → 401；存在但无效 → 403；错误码稳定，不回显 key。

### R4：Rate Limit

- 令牌桶：按 `route_id` + 身份（tenant/subject，缺省为远端 IP 哈希）计数；容量与补充速率可配。
- 状态有界：桶表设上限并淘汰最久未用；禁止无界增长。
- 超限 → 429，`Retry-After` 仅在能给出真实预算时设置。
- 时间推进用 `tokio::time`（测试用 `pause()`），不依赖墙钟。

### R5：集成与失败语义

- `gateway-server` 显式构造并注册 Filter；新增策略只改本 crate 与注册点，不动 `gateway-http` 路由算法。
- 认证失败为 fail-closed；限流状态异常（内部错误）fail-closed 并记 `Internal`。
- 策略执行在响应提交前完成；提交后不得改变状态码。

## Acceptance Criteria

- [ ] 配置校验：未知 `kind`、重复策略名、路由引用不存在、明文密钥字段均被拒绝，错误类别正确。
- [ ] JWT：合法、过期、签名错误、`iss`/`aud` 不匹配、缺 header 各有测试；错误断言具体变体。
- [ ] API Key：合法、缺失、错误 key、常数时间比较被使用（代码层断言或注释 + 单元测试）。
- [ ] Rate Limit：突发到上限、补充后恢复、按身份隔离、桶表淘汰、`Retry-After` 逻辑各有测试；测试用 `pause()` 推进时间。
- [ ] 顺序：同一路由上认证先于限流（用 mock 断言未认证请求不消耗令牌）。
- [ ] 日志不含 token/key；有专门测试格式化错误后不包含凭证。
- [ ] 路由未引用策略时行为不变（回归测试）。
- [ ] 基线五条命令全绿；`gateway-core` 依赖树不变。

## Out of Scope

- OAuth2/OIDC 发现、JWKS 轮换、RS256/ES256（后续按真实需求）。
- 分布式限流（Redis/外部计数器）、配额中心、按模型 token 计费。
- 策略热加载语义的额外机制（沿用配置热重载）。
