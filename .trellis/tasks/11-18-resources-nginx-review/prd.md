# 资源配置 nginx 对齐审查

## Goal

让熟悉 nginx 的使用者能准确理解 nico 的 `resources.yaml`，在迁移前识别会改变请求行为的差异，并据具体场景决定哪些名称、文档或能力值得调整。

## Background

`bootstrap.yaml` 的 `runtime`、`http.listeners[]`、`proxy.tls` 已完成对齐，任务记录位于 `.trellis/tasks/archive/2026-11/11-17-dual-listener/`。`.trellis/workspace/tincopper/journal-1.md` 的 Session 12 将资源配置审查列为下一项独立工作，完整迁移指南另列后续。

现行资源路由支持 host 与 path 匹配。nginx 官方文档明确区分 URI 规范化、`proxy_pass` URI 替换、速率与并发限额、总时长与 I/O 间隔等概念。对齐判断以当前实现和官方来源为依据。

## Requirements

### R1：建立 nico 现行契约的证据

覆盖路由 host/path 与冲突处理、上游身份与成员选择/健康、插件声明与作用域/阶段、请求限制与缓冲。每项主要断言引用源码或行为测试的位置；明确文档值和路由值何时覆盖、何时只能收紧。

### R2：给出有条件的 nginx 对应表

以 nginx 官方文档为依据，对照 server/location、proxy_pass、upstream、访问控制与限流配置。每行说明可直接映射的前提、存在的语义差异或当前无法表达的能力；同名策略不能未经验证就称为算法兼容。

### R3：形成可决策的调整建议

每项建议绑定具体配置或请求场景，说明用户价值、兼容性代价与验证方式。区分文档表述问题、候选命名调整和新增能力；涉及候选 schema 变化时说明现有配置、traffic/llm 构建、热重载与 LKG 的影响。

### R4：限定本次交付

本任务交付研究与建议，文件全部位于本任务目录；不修改产品代码、binding spec、README 或运行示例。报告可以得出“保留现有名称/结构”的结论，不以必须改 schema 作为完成条件。用户选定的产品调整和完整迁移指南另行规划。

### R5：验证结论可追溯

官方来源注明访问日期；内部断言带文件:行号或测试名；明确区分本轮执行的验证、静态阅读的测试和未验证项。旧任务仅作线索，最终报告自包含，不能要求读者先恢复旧对话。

## Acceptance Criteria

- [x] AC1（R1）：现行契约研究覆盖路由、上游、插件、请求限制四类，并有对应代码/测试锚点；见 `research/current-resource-contract.md` §2—§5。
- [x] AC2（R2）：对应表覆盖 host/listener、path/URI 转发、LB/健康、策略作用域和请求边界，逐项列出 nginx 官方来源与等价前提；见 `research/findings.md` §2 的 19 项对照。
- [x] AC3（R2、R3）：至少给出 host/path、路径转发、成员选择/健康、限流共享范围、body=0 / timeout 的可观察反例；见 `research/findings.md` §3 的 C1—C9，均区分推导与已运行验证。
- [x] AC4（R3）：建议按误配影响与代价排序，包含“保留/候选调整/独立能力缺口”的判断和验收方式；见 `research/findings.md` §4—§7。
- [x] AC5（R4）：改动只在 `.trellis/tasks/11-18-resources-nginx-review/`，不改变现行配置接受范围或请求行为；见 `research/verification.md` 的路径与产品零改动检查。
- [x] AC6（R5）：最终报告的主要断言均可追溯；验证结果与未验证项分别记录，无无条件的 nginx 兼容声明；独立检查通过，见 `research/check-review.md`。

## Out of Scope

- 新的 server/location 层级、nginx DSL 解析、配置自动转换或全功能兼容。
- schema/字段重命名、路由算法、健康状态机、插件执行或限流行为的实际修改。
- 新增 rewrite、正则路由、外部认证、并发限制或可信代理功能。
- 完整 `docs/nginx-migration.md` 迁移指南。
- 证书热加载、SNI 多证书、mTLS、metrics TLS。
- 将 LLM provider、协议转换、token 预算等概念机械套入 nginx 指令。
