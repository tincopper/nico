# Spec 知识评估

日期：2026-11-18。按 `trellis-update-spec` 评估，本任务只交付研究和建议，没有新增或改变签名、配置契约、运行行为或基础设施集成，未触发七节 code-spec 变更模板。

## 本任务的处理

PRD R4 限定全部改动位于任务目录，binding spec、README 和运行示例不在本次修改范围。当前契约的证据保存在 `current-resource-contract.md`，nginx 官方依据保存在 `nginx-resource-semantics.md`；最终决策建议和后续验收在 `findings.md`。

现有 `.trellis/spec/backend/database-guidelines.md` 已规定严格校验、原子发布/LKG、路由 identity 共用定义和配置字段必须有消费者；`.trellis/spec/backend/quality-guidelines.md` 已规定先证据后抽象、行为测试与文档责任。本次不为这些既有规则重复增加原则性文字。

对 `providers[].timeout_ms` 的后续核查直接受“字段必须有消费者”既有条款约束；研究阶段没有证明该字段如何影响运行时，因此不把待确认推论写成新的 binding contract。

## 后续可固化的具体知识

| 知识 | 为什么值得保留 | 后续归属与触发条件 |
|---|---|---|
| 平面 host+path 路由、重复规则拒绝、listener 不参与路由 | 防止把 nginx server/location 继承强加到现行配置 | 资源配置文档修订；若改变 matcher/作用域，再同步 backend HTTP 契约与断言 |
| route timeout 可解析为较大值但实际被文档上限截断 | 防止示例承诺无法生效的两分钟预算 | 文档/示例修订；若改为发布期拒绝，需单独规划错误矩阵、迁移与 LKG 测试 |
| buffers_body 只扣请求 body；convert 非流式响应另有整体缓冲 | 防止将插件接口限制误写成全网关无响应缓冲 | README/示例修订；若改缓冲实现，更新 backend HTTP/Pingora 契约 |
| 全局认证建立的身份可供后续全局 subject 限流使用 | 防止将初始空身份误当成整个 global scope 无身份 | 策略文档修订，配专门的 global auth→limit 场景验证；当前为调用链推导 |
| LB 同名策略、失败阈值与冷却并非 nginx 算法保证 | 防止只按字段名迁移 sticky keys、权重或失败窗口 | 完整迁移指南与字段说明；任何重命名或算法变化另行明确兼容性 |
| `ttft_timeout_ms` 当前限制每次 read | 防止部署者误以为首 token 后不再限制 | 命名与兼容性决策后同步 schema/spec/测试，不在审查中修改契约 |
| `providers[].timeout_ms` 初步未找到 runtime consumer | 现有“字段必须有消费者”规则要求核查真实数据流 | 独立核查并给可复现断言后再决定补消费或拒绝字段；本次不定为已复现 bug |

本次结论：保留研究证据与后续验收建议，不修改 binding spec。未来产品或文档任务应引用最终报告，并将已选定的行为与测试同步到归属规范。
