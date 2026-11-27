# 上游主动健康检查

## Goal

为文档 `upstreams[]` 的成员提供**可选的周期性主动健康探测**：即使没有真实流量，也能按周期
发现故障并在连续成功后恢复。缺省不启用，现有部署行为与配置保持不变。

这项能力对齐主流网关的主动检查模型：NGINX Plus 的 active health check 使用连续失败/成功
阈值；Envoy 的 `interval`、`timeout`、`unhealthy_threshold`、`healthy_threshold` 与
`no_traffic_interval` 是独立的主动检查语义。开源 NGINX 的 `max_fails`/`fail_timeout` 仍是
被动检测，不能作为本任务的完整模型。

## Confirmed facts and external comparison

- 当前 `nico-http` 的 `health.rs` 是被动状态机：`failures`、`ejected_at`、`probing_since`
  只由真实请求的 `admit()`/`record()` 改写；其模块文档明确称 ejection passive。
- 当前成员身份 `(upstream, name, address, port)` 全等继承，DNS 展开是发布期事实；
  `UpstreamPools` 跨 reload 保留 `MemberState`，`SelectionTable` 随 generation 不可变。
- 循环任务必须归 `nico-server` 的 Pingora `BackgroundService`，因为 CancellationToken 和
  tokio-util 只属于 server 层；nico-http 提供探测执行器与状态接口，不自行 spawn。
- LLM provider pool 没有 `UpstreamPools` 成员模型，provider 的 HTTP 200 也不能证明模型/凭证
  可用；本任务只处理文档 `upstreams[]`。
- NGINX Plus 官方 active health check 文档：`interval` 默认 5s、`fails`/`passes` 控制连续
  失败/成功、默认 2xx/3xx 通过、可用 `uri`/`match` 自定义；`mandatory` 可让启动初始状态为
  checking。官方页面明确这是商业功能：
  <https://nginx.org/en/docs/http/ngx_http_upstream_hc_module.html>
- NGINX 开源 upstream 文档：`max_fails`/`fail_timeout` 仅定义真实代理失败的被动检测：
  <https://nginx.org/en/docs/http/ngx_http_upstream_module.html>
- Envoy 官方 HealthCheck API：`interval` 为检查间隔，`no_traffic_interval` 处理尚无流量的
  集群，`timeout` 为单次上限，`unhealthy_threshold`/`healthy_threshold` 是主动连续阈值，
  HTTP `path` 必填、`method` 默认 GET、`expected_statuses` 为显式成功范围：
  <https://raw.githubusercontent.com/envoyproxy/data-plane-api/main/envoy/config/core/v3/health_check.proto>

## Requirements

### R1 配置面（resources.yaml，per-upstream，缺省关闭）

- `upstreams[].health_check` 可选块，字段：
  - `method`：GET 或 HEAD，默认 GET；
  - `path`：必填，`/` 开头的 bounded printable ASCII，禁 `?`、`#`、空白和控制字符；
  - `expect`：`non_5xx`（默认，任何非 5xx 响应通过）或 `2xx`；
  - `interval_ms`：主动检查间隔，`1..=600_000`；
  - `timeout_ms`：单次检查上限，`1..=600_000`；
  - `unhealthy_threshold`：连续失败次数，`1..=100`，默认 1；
  - `healthy_threshold`：连续成功次数，`1..=100`，默认 1；
  - `no_traffic_interval_ms`：可选；无真实请求期间的检查间隔，`1..=600_000`，省略时使用
    `interval_ms`。它借鉴 Envoy 的 no-traffic 语义，避免零流量时停探测。
- `RawUpstream` 使用 `deny_unknown_fields`；所有健康检查字段是 upstream 级别，不加到
  endpoint，保持与现有 ejection 参数相同的配置归属。
- `health_check` 缺省 = 不创建主动状态、不启动该 upstream 的探测，既有文档零行为变化。

### R2 主动与被动状态正交

- 不复用被动 `failures`、`ejected_at`、`probing_since` 作为主动检查计数或探测槽位。
- 在 `MemberState` 中保存独立的主动状态（示意）：
  - `active_health: { status: Initial|Healthy|Unhealthy, consecutive_failures, consecutive_successes, in_flight }`；
  - `in_flight` 保证同一成员同一时间最多一个主动检查；
  - 状态随成员身份继承，reload 不清零；健康检查配置变化按明确规则重置主动连续计数（默认
    采用配置变更后从 Initial 重新计数，避免旧阈值下的计数污染新阈值）。
- 被动 ejection 仍由真实请求 `admit()/record()` 独立控制；主动检查不抢占被动 half-open probe
  槽位，真实请求的被动 probe 也不改变主动 `rise/fall` 计数。
- 成员可选性为两者的交集：
  - 被动状态未 ejected；
  - 若启用主动检查，主动状态为 Healthy（启动 Initial 时按 `initial_status` 规则处理，见 R3）。
- 主动失败/成功只更新主动状态；当主动状态从 Healthy→Unhealthy 或 Unhealthy→Healthy 时，
  通过同一成员选择表的可选性与事件日志反映变化；不伪造 `gateway_upstream_ejected_total`，
  因为该指标专门表示被动 ejection 事件。

### R3 初始、无流量与恢复语义

- 默认 `initial_status = healthy`，以兼容 NGINX/Envoy 常见的非 mandatory 启动行为：第一次检查
  失败前不阻塞真实请求；如果部署需要启动前验证，后续独立扩展 `mandatory`，不在本任务加入隐式
  启动阻塞。
- 所有启用 `health_check` 的成员从第一轮开始按 `interval_ms` 探测；没有真实请求时不暂停，
  `no_traffic_interval_ms` 只允许更慢或相等的节拍以降低空闲流量。
- Healthy 状态：连续失败达到 `unhealthy_threshold` 才变为 Unhealthy；单次失败不立即剔除，
  除非阈值为 1。
- Unhealthy 状态：连续成功达到 `healthy_threshold` 才恢复为 Healthy；失败会清零成功计数。
- 进入 Unhealthy 后，主动检查仍继续按 interval 运行，直到达到 rise；不使用被动 cooldown
  作为主动恢复计时器。
- 探测响应：`non_5xx` 下 2xx/3xx/4xx 通过，5xx 失败；`2xx` 下只有 2xx 通过；连接失败
  与超时分别记 `connect`/`timeout`。不做 body、正则或自定义状态码列表。

### R4 探测器与生命周期

- 探测执行器属于 nico-http，但循环驱动属于 nico-server 的 Pingora `BackgroundService`
  `upstream-health`；持有 `Arc<HttpStateHandle>` 与进程 CancellationToken，nico-http 不新增
  tokio-util 依赖。
- 每个 tick 读取当前 generation 的 `SelectionTable`，为所有启用检查的 upstream/member
  调度一次检查；同一成员 `in_flight` 时跳过本轮，upstream 间可并发，单 upstream 的并发上限
  需有边界（默认串行）。
- 连接复用现有 upstream scheme/address/port 与 TLS trust policy；HTTP Host 使用文档 upstream
  的 logical host；method/path 来自 health_check；探测请求不经过插件、认证、限流、token budget、
  正常请求日志或正常请求指标。
- reload 时，旧 generation 在飞检查保留其成员 Arc 和该代 health policy；新 generation 通过
  成员身份继承 active 状态。被删除/地址变化的成员只在其最后一个 Arc 释放后消失。
- shutdown 取消新检查并等待/放弃在飞检查，不阻塞超过现有 drain 规则。

### R5 可观测性

- 新指标 `gateway_upstream_probe_total{upstream, endpoint, result}`，`result` ∈
  `ok`/`connect`/`timeout`/`5xx`/`status_mismatch`；标签只取配置名，使用 SharedString。
- 新增主动状态转换日志：Healthy→Unhealthy 为 warn，Unhealthy→Healthy 为 info，字段为
  upstream、endpoint、连续失败/成功计数和阈值；不输出地址、凭据或请求体。
- 现有 `gateway_upstream_ejected_total` 继续只统计被动 ejection；现有被动 ejection/recovery
  日志和 selected 指标不变。

### R6 明确排除

- LLM provider pool 主动探测；provider 独立健康模型另开任务。
- DNS 后台重解析、per-endpoint 探测覆盖、body/正则/任意状态码匹配。
- 探测认证/凭据、插件链、quota、正常请求日志和正常请求指标。
- mandatory 启动阻塞、跨副本健康状态共享、session affinity。

## Acceptance Criteria

- [ ] schema：`health_check` 读写、缺省关闭、字段边界、`healthy_threshold`、
      `no_traffic_interval_ms`、deny_unknown_fields 全覆盖。
- [ ] active 状态机：所有成员周期探测；Healthy 的 fall、Unhealthy 的 rise、失败/成功清零
      对方计数、同成员 in-flight 去重、初始 healthy、配置变更重置计数均有单测。
- [ ] passive 回归：现有 `admit/record` 测试全通过；主动检查不占用被动 probe 槽，
      被动 ejected 指标仍只统计真实请求触发的 ejection。
- [ ] 探测器：zero-traffic 仍运行；no-traffic interval 生效；shutdown 停止；reload 后新成员
      表与继承状态生效；同一成员不并发探测。
- [ ] 集成：mock upstream 无真实请求时，连续失败达到 fall → Unhealthy/不可选，修复后连续成功
      达到 rise → Healthy/回池；`non_5xx` 下 404 通过、`2xx` 下 404 失败。
- [ ] 指标/日志：probe_total 结果标签与主动状态转换日志正确；既有 ejected/selected 指标零改动。
- [ ] feature matrix、依赖边界、全树 ≤500 行、五条基线全绿。

## Out of Scope

- LLM provider 主动检查、DNS 后台解析、per-endpoint 配置、body/自定义 status 匹配。
- 探测认证/插件/quota/请求日志；mandatory 启动阻塞；跨副本状态共享。

## Key decisions

- 主动检查按 interval 覆盖所有启用成员，不能只探测已 ejected 成员；这是零流量发现的必要条件。
- 主动 `fall/rise` 状态与被动 ejection 正交；不复用 `max_fails/fail_timeout_ms` 的 streak/half-open 槽。
- 默认 initial healthy，缺省关闭 health_check；不引入 mandatory 启动阻塞。
- 配置放文档 upstream，循环由 server BackgroundService 驱动，provider pool 不适用。
