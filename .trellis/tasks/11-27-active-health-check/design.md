# 设计：上游主动健康检查

## 1. 设计目标与状态模型

NGINX Plus 和 Envoy 的主动健康检查都把周期探测建模为**独立的连续失败/成功序列**：主动检查有自己的
interval/timeout/fall/rise 和响应判定；被动检查则从真实代理请求学习。Nico 保留现有被动状态机，
另加主动状态，最终可选性取两者交集。

```text
                  active check: fall consecutive failures
     ActiveHealthy ------------------------------------------> ActiveUnhealthy
          ^                                                        |
          |       active check: rise consecutive successes         |
          +--------------------------------------------------------+

     PassiveClosed -- max_fails real-request failures --> PassiveEjected
          ^                                                   |
          | real answer / passive half-open probe              | fail_timeout_ms
          +---------------------------------------------------+

     selectable = ActiveHealthy (or not configured) AND PassiveClosed
```

主动检查每个 interval 探测所有启用的成员，包括当前没有真实流量的 upstream。主动检查不认领被动
`probing_since`，被动请求也不改变主动 `fall/rise` 计数；这样两套来源不会争同一个 half-open 槽位，也
不会把探测结果记成真实请求的 `gateway_upstream_ejected_total`。

## 2. Invariants 与失败模式

| 不变量 | 违反时的失败模式 |
|---|---|
| active status 与 passive `Health` 正交 | 探测占用 `probing_since` 或真实请求清掉 fall/rise，状态归因与恢复会随机漂移 |
| 所有启用成员周期探测，包括无流量成员 | 只探测 ejected 成员无法发现从未被真实请求踩中的故障，违反产品目标 |
| fall/rise 是连续序列且各自清零对方 | 失败与成功交错仍累计会造成误剔/误恢复 |
| 启动初始状态是显式定义的 | 初始 `Unknown` 与 “只有 Healthy 才可选” 可能让配置发布后 upstream 长时间无流量 |
| 一个 member 同时至多一个 active check | 慢探测重叠会积压 TCP 连接并以乱序结果覆写状态 |
| reload 继承由 MemberId 决定 | 地址变化的机器继承旧机器的健康结论 |
| Pingora/TLS 的连接规则复用，不自建第二套 trust policy | 探测通过而真实请求因不同 TLS/SNI/Host 失败，形成假健康 |
| probes 不进入业务请求记录、插件、quota、普通 request metrics | 探测会污染用户日志、token budget 与请求量指标 |
| 指标标签只来自文档名字 | 每成员每结果标签保持配置有界，不受请求影响 |

## 3. Domain model 与成员状态

### 3.1 配置模型

在 `nico-core` upstream document model 增加：

```rust
pub struct HealthCheck {
    method: ProbeMethod,             // Get | Head, default Get
    path: Arc<str>,                  // required when section present
    expect: ExpectedStatus,          // Non5xx (default) | S2xx
    interval_ms: u32,
    no_traffic_interval_ms: u32,     // defaults to interval_ms
    timeout_ms: u32,
    unhealthy_threshold: u8,         // fall; default 1
    healthy_threshold: u8,           // rise; default 1
}
```

`initial_status` 不公开配置：固定为 Healthy，与 NGINX/Envoy 的非-mandatory 常见启动行为一致，
避免新配置一发布就把所有成员挡在流量外。主动阈值的缺省 1 表示部署写出 `health_check` 后
第一次失败/成功立即反映；需要抗噪的部署把阈值调高。`no_traffic_interval_ms` 只在 upstream
过去一个 `interval_ms` 没有真实请求时使用，缺省等于 `interval_ms`，所以缺省仍是零流量持续探测。

范围和文档验证：

- interval/no-traffic interval/timeout：`1..=600_000` ms；阈值：`1..=100`；
- method 是封闭 enum GET/HEAD；expect 是封闭 enum `non_5xx`/`2xx`；
- path 以 `/` 开头、最多 1024 bytes printable ASCII；拒绝 `?`/`#`/空白/控制字符；
- `health_check` 对 endpoint/upstream pool member 写在 upstream 本身，所有成员共享探测策略；
- 当前不支持 query string，避免 path 在 YAML 与 URL 构造时产生双重解析。

### 3.2 运行态

`MemberState` 增加独立的 `ActiveHealth`（锁域与被动 `Health` 分开）：

```rust
struct ActiveHealth {
    condition: Mutex<ActiveCondition>,
}
struct ActiveCondition {
    status: ActiveStatus, // Healthy | Unhealthy
    consecutive_failures: u8,
    consecutive_successes: u8,
    in_flight: bool,
    last_probe_at: Option<std::time::Instant>,
    last_traffic_at: Option<std::time::Instant>,
}
```

状态只由探测器调用 `begin_probe` / `finish_probe` 写；真实请求只在 request start 标记
`last_traffic_at`，不写主动健康判定。锁内不做 I/O、分配或 await。主动阈值在发布 generation
上生效；同 MemberId 继承计数的前提是旧、新 `HealthCheck` 完全相等，配置变化（含从 None 到
Some、Some 到 None）重置主动状态到 initial Healthy，避免把旧 interval/threshold 下的 streak
解释为新策略下的 streak。被动 Health 的现有继承规则不变。

最终 member admission：

- `HealthCheck = None` → active 子状态视为 Healthy（legacy path，无额外分支可改变旧行为）；
- `Some` → active status 必须为 Healthy；
- 两者均须满足现有 passive `Health::available/admit`。

被动 failure 不改变 active counters/status；主动 failure 不改变 passive `failures/ejected_at/
probing_since`。这使 `max_fails/fail_timeout_ms` 与 `unhealthy_threshold/healthy_threshold` 的
可观察含义保持独立，两个系统不会相互清零或把一个来源的失败归给另一个来源。

## 4. Probe I/O

### 4.1 探测请求

一次 probe 从某代 `SelectionTable` 快照拿到：upstream `scheme`、logical `host`（Host 与 TLS SNI）、
成员 `address/port`、health policy 和成员的 `Arc<MemberState>`。Probe 用同一 `Destination`/
peer 构造入口和 TLS policy 建连接；不能在健康检查里另建信任根、关闭证书校验或把成员地址放进
Host/SNI。

HTTP 探测只发 method + path + Host，不发送 credential、API key、用户 headers、body、插件链或
请求级 token budget。只读 response head，按 `ExpectedStatus` 得到 pass/fail，然后立即关闭探测流；
不能把任意 upstream response body 拉入内存。

**实现前 capability gate**：Pingora 0.9.1 `nico-http` 目前只有 ProxyHttp outbound peer 路径，未见
独立 HTTP client API 的项目封装。阶段 3 必须用 pinned Pingora 源码验证能否复用 client protocol
构造与 TLS roots；若只能直接用 `TcpStream`，HTTPS 主机名验证仍需严格实现，禁止为了让 probe 工作
而跳过 TLS 验证。若 Pingora 没有可复用的 client seam，执行计划应在代码实现前停在 capability
finding，由产品规划单独决策是否引入独立 probe transport（这会改变依赖/信任边界）。

### 4.2 分类与阈值

- connect 失败 → `ProbeResult::Connect`；deadline → `Timeout`。
- 收到响应头后：`non_5xx` 下 200–499 成功、500–599 失败；`2xx` 下仅 200–299 成功；其它状态
  → `StatusMismatch`。
- 一个 check pass：失败计数清零，成功计数递增；达到 `healthy_threshold` 后状态变 Healthy。
- 一个 check fail：成功计数清零，失败计数递增；达到 `unhealthy_threshold` 后状态变 Unhealthy。
- 仍处于 Healthy/Unhealthy 的重复结果不发状态转换日志；每次实际 probe 都递增
  `gateway_upstream_probe_total`。
- 每成员最多一个 in-flight probe。超时后必须 drop/close session 并释放 in_flight；进程 shutdown
  取消新 probe，并有界等待/取消在途 probe。

## 5. Probe scheduler 与生命周期

### 5.1 Ownership

- `nico-http::UpstreamHealthProbe` 提供一次 `run_once(current_generation, now)`，负责枚举启用的成员、
  决定 due、claim、发探测、report 状态与 emit probe result。
- `nico-server::HealthService` 是 Pingora `BackgroundService`：持有 `Arc<HttpStateHandle>` 和唯一
  `CancellationToken`；循环在同一个 Pingora runtime 运行，不引入 nico-http 的 spawn/token 依赖。
- `HttpStateHandle` 持进程级 `Arc<ProbeSchedule>`/probe runtime facts，reload 时只更新当前 generation
  的成员视图；不新建第二份成员健康表。

### 5.2 Scheduling

- 每轮遍历当前 generation 中 `health_check: Some` 的成员；不是只遍历 Unhealthy。
- 每成员根据 last probe time 决定 due：有真实流量用 `interval_ms`；超过 `no_traffic_interval_ms`
  未见真实流量时按 `no_traffic_interval_ms`；默认两者相同。
- 同一 tick 的首轮探测做确定性摊开（例如以 hash(upstream, endpoint) 在一个 interval 中分桶），避免
  重启后全成员同时建连；同 upstream 默认串行，upstream 间并发有硬上限。
- 每次调度重新读取当前 generation，避免长期持有旧 Table；已开始的 probe 可持旧 generation 的
  `Arc<MemberState>` 完成。若成员被 reload 移除，结果只写回该旧 Arc，旧 Arc drop 后自动释放。
- Shutdown：循环在 `CancellationToken::cancelled()` 中止；在途 probe 有单次 `timeout_ms` 上限，
  shutdown 后不再排新任务并等待最多该 timeout 加一个短 join grace。

### 5.3 Reload 与状态继承

Publication 对每个成员做 MemberId 精确匹配。active state 的继承条件：旧/新都启用 health check，且
参数值相等；否则 active state 初始化为 Healthy、计数 0。被动 Health 仍按原 MemberId 继承，与 active
配置变化无关。in-flight probe 持有旧状态 Arc，完成后不写新成员（除非两代本来继承了同一个 Arc）。

## 6. Metrics 与 logging

- `gateway_upstream_probe_total{upstream, endpoint, result}`：`result=ok|connect|timeout|5xx|status_mismatch`；
  每次完成/失败的实际 probe 一次。标签用 upstream/endpoint 文档名；不加 path、地址、请求 id。
- Active Healthy→Unhealthy：`warn!`，字段 upstream/endpoint、连续失败数、threshold；不增加
  `gateway_upstream_ejected_total`（该 counter 专指 passive `max_fails` 事件）。
- Active Unhealthy→Healthy：`info!`，字段 upstream/endpoint、连续成功数、threshold。
- 每次 probe 结果不写普通 access log、`gateway_http_requests_total` 或 `gateway_http_request_seconds`。

## 7. Compatibility, rollout and rollback

- 不写 `health_check` 的文档不创建 active state，不运行 probe，既有被动选择结果逐位不变。
- 功能随资源文档原子发布；无效 health_check 拒绝候选并保留 LKG。
- Feature 不加新 crate，不加 nico-http cancellation dependency；BackgroundService 的装配由 server
  在配置含至少一个 health_check 时启用。
- Rollback：从资源文档移除 health_check，下一代不再安排探测，成员回到纯 passive；无需数据迁移。

## 8. Testing

- Schema：字段、默认、范围、unknown field、坏 path 和 expect 值。
- Active state：fall/rise、成功/失败互相清零、initial Healthy、single in-flight、配置变化重置、
  member identity 变化不继承。
- Passive regression：既有 Health tests 不变；主动 probe 不占被动 slot、不改 passive counters；
  被动 ejected 时 active 成功不会擅自清除 passive ejection。
- Probe executor：HTTP method/path/Host/SNI、响应状态分类、TLS 证书错误、deadline/drop、body 不读取、
  不发敏感 headers。
- Scheduler：no traffic 仍探测、no_traffic interval、错峰、有界并发、reload、shutdown。
- End-to-end：真实 mock upstream 无用户流量时 fall 到 Unhealthy、修复后 rise 回 Healthy；不同
  `expect` 判定；探测流量不出现在请求日志/业务指标中。
- 全量 workspace gates、feature matrix、dependency boundaries、500-line check。
