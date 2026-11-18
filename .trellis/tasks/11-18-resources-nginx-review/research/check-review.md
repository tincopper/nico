# 独立检查记录：资源配置 nginx 对齐审查

检查日期：2026-11-18。结论：**通过，已修正发现的局部证据/表述问题；通过范围为研究报告交付，不表示 nginx 运行兼容或全部反例已实测。**

## 核对范围与方法

- 已读取 prd.md、design.md、implement.md、check.jsonl、findings.md 及清单全部六项来源。quality-guidelines.md（含 Testing Requirements / Code Review Checklist）、current-resource-contract.md（含 §5–7）和 nginx-resource-semantics.md 均已读完；未以自动注入截断内容代替全文。
- 逐行检查 19 项对应表的 nico 契约、映射前提及官方来源；逐项检查 C1–C9 的可观察行为与后续验收。回读 matcher/重复校验/多 listener service、预算/read timeout、picker/健康状态机、插件排序/身份/分桶、buffer/head 改写、LLM transform/passthrough、provider 编译及 publication/traffic role 的关键源码与相关测试断言，未重新探索全库。
- 重新访问 nginx 官方 request_processing、upstream、core、proxy、limit_req 文档；C5 另定点读取官方 nginx 1.28.0 发行包中的 round-robin 源码。其余 nginx 边界以 nginx-resource-semantics.md 的官方来源取证为依据，未运行 nginx。
- git status 与 git diff HEAD 检查显示仅本任务目录未跟踪，现有 tracked 产品文件无 diff。本检查仅修改 research/findings.md、新增本记录，不修改状态、其他任务文件、产品代码、binding spec、README 或运行示例；未执行 add/commit/archive/切分支。

## 发现与修正

1. **C5 对 nginx 成功清零条件的前提不足。**原“30s 内失败/成功/失败/失败”仅凭官方参数文档不能推出特定实现的失败计数；也不能泛称窗口累计且成功不清零。已将对应表改为官方时间窗描述，C5 明确限定 nginx 1.28.0、默认 RR、至少两成员、单 worker、顺序且无重载，给出 t=0/1/2/3s 的同成员连接失败/200/连接失败/连接失败。静态源码显示选择时超过 fail_timeout 才前移 checked，失败同时更新 accessed/checked，成功仅在 accessed < checked 时清零，故限定序列可区别 nico 的每次非5xx清零。也明确跨窗后新选择的成功能够清零；没有引入“跨窗连续失败必不累计”或“严格滑动窗口”的未证结论。
   - 外部证据：https://nginx.org/download/nginx-1.28.0.tar.gz ，包内 src/http/ngx_http_upstream_round_robin.c:736–740、774–776、819–848，标明版本、访问日期与静态证据性质。未把它扩展到所有 picker/版本/并发情形。验收补充窗内/跨窗成功、目标成员与隐藏重试计数检查。
2. **两个锚点不够准确。**对应表重复匹配/tie 的测试锚点从 route/tests.rs:154 调整到 :144（测试及不可达说明）；C1 的 host-specific 短前缀优先级锚点从 :19 调整到 :64（直接相关测试）。事实本身成立。

## 重点反驳结果

- host 路由为同一平面候选循环，限定 host 没有 path 命中时可落无 host 项；全部 listener 共用 service/state。合法 document 拒绝重复匹配，不能教部署者靠文档顺序解 tie。C1 第一负例及完整跨 listener 矩阵仍如实标注未有专门 E2E。
- route timeout 可写大但运行取 min，起点不重置；ttft 消费为每次 read（含非流式/chunk 间隔）。报告保留所有 I/O 恰在 deadline 精确终止未验证的边界，没有将预算意图写成完整停滞保障。
- global auth→subject limiter 可从排序、同一 context 身份读写与请求链推出，专门 E2E 未找到；同实例跨 route 的桶键隔离有直接静态测试断言。nginx limit_req 漏桶及 limit_conn 并发概念分开。
- buffers_body 只扣 body、头先发有 E2E 源码断言；convert 非流式（含错误文档）有 bounded buffer；passthrough 的 body 不解码并不阻止 provider path/header 覆盖。
- providers.timeout_ms 仅列已读链未传递且未找到 runtime consumer/生效测试，保持待确认，不判为已复现 bug，不补消费或删字段。

## AC 对应

| AC | 结果与依据 |
|---|---|
| AC1 | 通过：current-resource-contract.md §1–5 覆盖路由、上游、插件、请求边界；报告对应表及反例保留代码/测试锚点，关键消费路径已回读。 |
| AC2 | 通过：19 项覆盖 host/listener、path/URI、LB/健康、策略作用域、限制与缓冲；每项有官方 URL 和映射前提/无法表达的边界，未以同名宣称算法兼容。 |
| AC3 | 通过：C1–C9 可通过目标 origin、request-target、选择分布/后续可用性、接收/拒绝、时点/断流等观察；C5 已限定版本和事件前提，全部明确不是本轮实进程差分结果。 |
| AC4 | 通过：D1–D6 按影响/代价排序；N1 保留资源结构及健康模型、N2 候选 read_timeout、N3 待确认消费、G1–G4 独立能力分开。共同约束与各项影响/验收覆盖旧 YAML、traffic/llm、reload/LKG、在途 generation。 |
| AC5 | 通过：仅任务研究记录；未改变配置接受范围或运行行为。未引入新产品规范。 |
| AC6 | 通过：主要断言可追溯，官方滚动文档/版本源码、测试静态断言、既有实际测试记录与未验证事项分开。修正后无待处理的阻断性报告错误。 |

## 验证与剩余限制

既有 verification.md 记录 core 236 + http/lb/plugins 593 = 829 项库测试通过，本检查仅核对记录与相关断言，**没有重跑**，没有声称执行 server 集成测试。未改 Rust，无具体新运行疑问需要本轮全量构建、fmt/clippy/doc/feature matrix 或新增测试。文档空白、结尾换行与对应表/反例数量在本检查收尾时验证；JSONL 与全量锚点最终检查由主代理统一收口。

仍未验证：nginx 实进程差分（含 C5 受控序列）、global subject/跨 scope 双执行专门 E2E、特殊 URI/完整 listener 路由矩阵、provider timeout 消费；以及已列明的 DNS 动态多地址、已有 ejection 热改 max_fails=0、并发健康结果、各类 I/O 停滞即时 deadline、插件扩张峰值内存。上述均保持证据限定，并有对应后续验收，不阻断本次研究交付。命名候选与能力实施仍需另行选择，本检查未授权任何产品变更。
