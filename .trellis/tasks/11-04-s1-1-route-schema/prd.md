# 路由 schema 扩展与四段校验

## Goal

把 `gateway-core::Document` 从 `apiVersion + limits` 扩展为完整的路由配置模型（`routes` / `upstreams` / `providers`），并把校验固定为「解析 → 引用校验 → 语义校验 → 构造」四段，补齐文档边界（数量、深度、节点数）。这是阶段 1 其余三个子任务的唯一配置模型来源。

## Dependencies

- 无前置子任务；本任务是阶段 1 的第一步。
- 后续：`1.2 http-data-plane`（消费本模型并编译路由表）、`1.3 gateway-plugins`（在其上增加策略引用）、`1.4 gateway-ai-passthrough`（消费 `providers`）。

## Requirements

### R1：配置模型

`apiVersion: gateway/v1` 保持兼容（新增字段是追加式），文档结构：

```yaml
apiVersion: gateway/v1
limits:
  max_request_body_bytes: 1048576
  request_timeout_ms: 30000
providers:
  - name: openai-primary
    kind: openai
    base_url: https://api.openai.com
    credential_ref: env:OPENAI_API_KEY
upstreams:
  - name: api-backend
    scheme: http
    host: 127.0.0.1
    port: 8080
routes:
  - id: api-public-v1
    path_prefix: /api/
    upstream: api-backend
  - id: ai-chat
    path_prefix: /v1/chat/
    provider: openai-primary
    timeout_ms: 120000
```

- 每条路由必须且只能引用 `upstream` 或 `provider` 之一；两者都缺或都给 → 引用校验失败。
- `provider.kind` 本阶段只接受 `openai`；未知 kind 在解析阶段被 serde 拒绝
  （`DocumentError::Yaml`，类别 `Client`），不静默忽略。
- 路由字段：`id`、`host`（可选，精确匹配）、`path_prefix`（必填，以 `/` 开头）、`upstream` / `provider`、`timeout_ms`（可选，区间内）、`max_body_bytes`（可选，`1..=文档上限`）。
- `upstreams[].host` 非空、无 scheme/端口/空白，长度 ≤ 253。
- `upstream.scheme` 只接受 `http` / `https`；`port` 必填；`host` 非空。
- 文档级结构继续 `#[serde(deny_unknown_fields)]`，不使用 `serde(default)`。

### R2：四段校验

1. **解析**：大小上限、UTF-8、YAML 语法、未知字段、类型错误。
2. **引用校验**：`upstream` / `provider` 名字存在；`route.id` 全局唯一；`upstream` / `provider` 名字唯一。
3. **语义校验**：`path_prefix` 以 `/` 开头且不含 `..`；`scheme` 合法；`port` 非 0；`timeout_ms` 在 `1..=600_000`；`max_body_bytes` ≤ 文档 `limits.max_request_body_bytes`；同 `(host, path_prefix)` 组合不得重复。
4. **构造**：全部通过后才构造 `Document`；任何阶段失败都不产生可发布文档。

### R3：文档边界与 bootstrap 接线

- `ParseLimits` 由 `max_bytes` + `SchemaBounds`（`max_routes`、`max_upstreams`、`max_providers`、
  `max_nodes`、`max_depth`）组成；`SchemaBounds` 实现 `Default`，默认值是唯一来源。
- 超限分别报独立错误变体（不是笼统 `TooLarge`）。
- 深度/节点数上限在本任务实现，替代阶段 0 留下的 "YAML 别名膨胀" 待办。
- 这些上限可由运维覆盖：`config/bootstrap.yaml` 增加**可选** `schema_bounds` 段，可整段省略，也可只写部分字段；
  省略的字段取 `SchemaBounds::default()`，未省略的字段覆盖默认值。`gateway-server` 用组装后的
  `ParseLimits` 启动管线。
- bootstrap 仍是不可信输入：未知字段、0 值、越界值在启动阶段失败并返回明确退出码；不存在的字段才是默认值，
  拼错的字段不能被默认吸收。

### R4：错误与分类

- 新增 `DocumentError` 变体：`ReferenceUnknown { kind, name }`、`DuplicateId { kind, id }`、`RouteTargetMissing { id }`、`RouteTargetAmbiguous { id }`、`TooMany { kind, count, limit }`、`TooDeep { depth, limit }`、`InvalidField { field, reason }`。
- 每个变体映射 `ErrorCategory`：`ReferenceUnknown` / `DuplicateId` / `RouteTarget*` / `InvalidField` → `Client`；`TooMany` / `TooDeep` → `Limit`。

## Acceptance Criteria

- [x] 示例文档（含普通 API 路由与 AI 路由）可解析通过，且被文档测试覆盖。
- [x] 未知字段、错误 `apiVersion`、非法 UTF-8、超 `max_bytes` 仍按阶段 0 行为被拒。
- [x] 引用校验：未知 upstream/provider、路由缺目标、路由给两个目标、重复 route id、重复 upstream/provider 名各有测试与具体错误断言。
- [x] 语义校验：`path_prefix` 不以 `/` 开头、含 `..`、非法 scheme、非法 provider kind、port 0、timeout 越界、`max_body_bytes` 超过文档上限、重复 `(host, path_prefix)` 各有测试。
- [x] 边界：`max_routes` / `max_upstreams` / `max_providers` / `max_nodes` / 深度超限各有测试，且错误类别为 `Limit`。
- [x] proptest：任意字节输入 `Document::parse` 不 panic（沿用阶段 0 用例并扩展）。
- [x] bootstrap 接线：省略 `schema_bounds` 时使用 `SchemaBounds::default()`；只写部分字段时其余取默认；
      全部字段都成功传到 `Document::parse`。
- [x] bootstrap 校验：未知字段在解析阶段被拒；每个 `schema_bounds` 字段的 `0` 与「超过策略上限」各一例被拒，
      且错误消息引用的上限与实际生效的上限一致（`BootstrapError::InvalidField`）。
- [x] 端到端：bootstrap 把 `max_routes` 设为 1、文档含 2 条路由时，管线拒绝该文档并保留 LKG
      （`degraded`，generation 不变）。
- [x] 现有 127 个测试全部仍通过；`gateway-config` 无行为改动（仅把 `ConfigDigest` / `Revision` /
      `RevisionOrdering` 的重导出指向拥有者 `gateway_core::version`），`gateway-server` 只新增
      `schema_bounds` 接线（`bootstrap.rs` + 构造 `ParseLimits` 处）与测试夹具中的必填段，其余行为不变。
- [x] `cargo +nightly fmt` / `clippy -D warnings` / `test` / `doc -D warnings` / `deny` 全绿。

## Out of Scope

- 路由匹配算法与 `CompiledRoute`（属 `1.2 http-data-plane`）。
- 策略（filters/policies）引用字段：由 `1.3 gateway-plugins` 任务按需要追加，本任务不预置。
- 任何网络代码或 `gateway-http` 变更。
- 配置文档拆分多文件、include、锚点复用。
