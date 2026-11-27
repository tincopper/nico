# 主动健康检查 E2E 补强

## Goal

补齐 `11-27-active-health-check` 交付的验收缺口：单测覆盖了状态机与探测执行器，但 **server 全链路**
（真实进程 + 真实 mock upstream + 真实探测往返）还没有证据。本任务用现有 HTTP 场景套件
（`RunningServer` + `MockUpstream`）证明：零真实请求下，`health_check` 配置驱动探测把故障成员剔除、
修复后恢复、probe 指标出现且请求指标不受污染。

## Background

- `serve_with` 已装配 `upstream-health` BackgroundService；`Resources` builder 尚无 `health_check`
  字段渲染，需要给 `UpstreamEntry` 加一个 `with_health_check` 步骤。
- 探测的前提是 upstream 有真实监听：`MockUpstream::start(Reply::…)` 就是现成的成员监听，探测请求会
  作为 `ObservedRequest` 出现在 `observation()` 里——这是「探测真的发生了」的证据。
- `RunningServer` 通过 `TestSource` 发布文档；测试可用第二次发布改变 mock 的 Reply 或阈值，
  验证 reload 后按新表探测。
- 指标经 `write_bootstrap_with_metrics` + 对 metrics listener 的 GET 抓取。

## Requirements

- R1 `Resources::health_check` builder 步骤（fall/rise/interval/expect 的场景可变性），渲染到
  `UpstreamEntry`。
- R2 新集成测试 `crates/nico-server/tests/upstream_health.rs`，场景：
  1. **零流量剔除与恢复**：两成员 upstream + `expect: non_5xx` + fall=2/rise=2 + 短 interval；
     成员 A 的 mock 持续回 5xx、成员 B 回 200。**不发任何真实请求**。断言：A 的 observation
     收到探测请求（证明探测在跑）；A 在两个探测失败后从 `gateway_upstream_probe_total`
     累计 `5xx` 且 upstream 进入 Unhealthy（通过探测结果标签和后续行为证明）；B 保持可选。
     然后 A 的 mock 换成 200 Reply，连续两个探测通过后 A 恢复（Rose 日志级别无法直接断言，
     通过 probe_total 的 ok 计数与后续客户端请求同时打到 A、B 证明回池）。
  2. **expect 语义**：`expect: 2xx` 下一个回 404 的成员被判失败（status_mismatch 计数），
     `non_5xx` 下同成员判 ok——两次发布或两个 upstream 分别断言。
  3. **被动互不污染**：主动剔除的成员不产生 `gateway_upstream_ejected_total` 样本（该指标
     仍只统计真实请求触发的 ejection）。
- R3 断言手段：probe_total 指标（metrics listener 抓取）、MockUpstream observation 的探测请求
  （method/path/Host 头）、真实客户端请求的落点分布（恢复后 A 服务流量）。
- R4 稳定性：interval 用测试级小值（如 100-200ms），等待用有界轮询（复用现有 wait helper 模式），
  不使用固定 sleep 断言。

## Acceptance Criteria

- [ ] `upstream_health.rs` 三场景全绿；探测请求在 observation 中带正确 method/path/Host。
- [ ] probe_total 的 result 标签（ok/5xx/status_mismatch）按场景出现；`gateway_upstream_ejected_total`
      在零流量场景保持 0 样本。
- [ ] 恢复后真实请求落到原故障成员（回池证明）。
- [ ] `Resources::health_check` builder 步骤渲染正确（单独断言或由场景覆盖）。
- [ ] 全树 ≤500 行、五条基线全绿。

## Out of Scope

- TLS 成员的探测 E2E（执行器单测已覆盖 Host/SNI 语义；真实 TLS 往返是后续任务）。
- reload 换表场景（probe 执行器单测已覆盖成员对齐；集成级 reload 组合是后续任务）。

## Open Questions

（无。场景与断言手段由现有套件能力决定。）
