# 执行计划：资源配置 nginx 对齐审查

## 开始条件

- [x] 用户批准创建任务并进入规划。
- [x] 初始需求写入 `prd.md`。
- [x] nginx 官方语义取证写入 `research/nginx-resource-semantics.md`。
- [x] nico 现行契约研究完成，关键断言有源码/测试锚点。
- [x] `prd.md`、`design.md`、本计划和两份上下文清单通过规划检查。
- [x] 用户在最终规划摘要后明确批准执行（“按次方案实施”）；已运行 `task.py start`，状态为 `in_progress`。

## 1. 建立现行证据表

- [x] 阅读研究文件，并对最终结论将引用的关键断言回读源文件。
- [x] 区分源码事实、测试静态证据、本轮实际执行结果与尚未验证事实；见最终报告 §1、§8。
- [x] 找到 config/README/旧调研中的不准确映射，记录 owning 文档位置；产品代码异常另列待确认项，未直接修改。

检查点 A：路由、上游、插件、请求限制四个维度均有可定位证据，不能以 nginx 同名字段填补 nico 缺失事实。

## 2. 形成最终报告 `research/findings.md`

- [x] 逐行对应表：19 项 nginx 概念、nico 字段、等价条件、主要差异、代码锚点及官方来源。
- [x] 给出 9 个配置/请求反例，覆盖 host/path、URI 转发、权重/健康、限流作用域和 body=0 / timeout。
- [x] 给出命名“保留/候选调整”结论，不为名称一致而引入新的行为暗示。
- [x] 记录文档/示例失真与真正能力缺口，分别建议后续边界和验收。
- [x] 对 schema 候选变更列出迁移代价、traffic/llm 影响、reload/LKG 约束；主要结构与现有名称建议保留。

检查点 B：读者能据报告判断已有配置是否可直接迁移，以及不能直接迁移的具体原因。

## 3. 有针对性验证

- [x] 运行能支撑争议结论的既有库测试，记录实际命令/结果；无争议的事实回读源码即可，不新增镜像实现的测试。规划阶段已执行 core/http/lb/plugins 库测试，合计 829 项通过，记录见 `research/verification.md`；后续仅在新发现需要时补跑。
已执行的相关库测试覆盖以下 crate 范围；没有产品代码变化，不重复运行：

```bash
cargo test -p nico-core --lib --locked
cargo test -p nico-http --lib --locked
cargo test -p nico-lb --lib --locked
cargo test -p nico-plugins --lib --locked
```

- [x] 相关库测试无阻塞；未执行的 nginx 差分、server E2E 与其他验证边界在报告 §8 明确列出。

本任务只改研究和规划 Markdown/JSONL，不要求运行全量 Rust fmt/clippy/doc/feature matrix；如果经用户批准新增 Rust/schema/spec 变更，则先修订本任务范围和验证计划，再按 backend/index.md 完整门禁验证。

## 4. 质量检查与收口

- [x] `trellis-check` 完成独立检查并通过；已限定 C5 健康反例的 nginx 版本/成功清零条件，修正两处测试锚点，记录见 `research/check-review.md`。
- [x] 验证清单与空白错误：

```bash
python3 ./.trellis/scripts/task.py validate .trellis/tasks/11-18-resources-nginx-review
git diff --check
git status --short
```

- [x] 确认改动仅位于 `.trellis/tasks/11-18-resources-nginx-review/`；产品代码、spec、README、config、deploy 均无 diff。
- [x] 按实际证据勾销 PRD AC1—AC6；产品命名、schema 与新增能力仍是后续选择，不是本报告的未完验收。
- [x] 按 `trellis-update-spec` 评估知识归属；见 `research/spec-assessment.md`。本任务只产出建议，未改变契约，binding spec 保持现状。
- [x] 报告与验收交付准备完成；拟议提交：`docs(config): review nginx resource semantics and migration gaps`。本轮用户请求执行报告，未请求提交；不运行 commit 或 archive，任务状态保留 `in_progress` 等待用户收尾。

## 回退点

- A 前：证据不全则继续取证，不推断等价关系。
- B 前：结论无法证明则标未验证或删去过度结论，不修改现行实现来配合报告。
- 用户选定 schema/行为变更：回到规划，重写受影响的 PRD/design/implement 并重新审阅。
- 本任务记录可以单独撤回；不得回退用户已有产品工作。
