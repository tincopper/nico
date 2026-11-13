# 异构 Provider 语义调研

## Goal

把 roadmap 上「异构 Provider（非 HTTP/JSON 原生、或需要本地模型服务器语义的 kind）：语义待澄清」
这个阻塞项，拆成可独立决策的三类，并给出每一类的边界与改动面估算。**产出调研与决策建议，
不写实现代码。**

## Background

- `11-03-unified-gateway-analysis/implement.md` §3 把它列为阶段 3 未交付项，§4 又写了
  「新协议使用独立 crate/binary 和协议原生 trait，不穿透 HTTP Filter」——两处措辞互相没有对齐。
- 现有 `Provider` trait（`nico-llm/src/provider.rs:192`）的四个实现（openai / openai_responses /
  anthropic / gemini）都是远程 HTTP/JSON API，方言不同但共享同一个 `Endpoint` 形状。
- `Endpoint = Destination + path + headers + model`（`provider/endpoint.rs:91`），`base_url` 必须
  是绝对 `http(s)` URL，`Scheme` 只有 `Http`/`Https`。

## Requirements

### R1 把「异构」拆成三类，分别给结论

- **方言异构**：同一 HTTP 传输、不同 JSON 方言。判定它是否已被现有契约覆盖。
- **协议异构**：同一 HTTP 传输、非 JSON/非请求响应式的 wire（gRPC、WebSocket、multipart）。
  定位扩展点与前置条件。
- **传输异构**：非 HTTP scheme（`grpc://`、`unix://`、进程内调用）。判定是否属于本项目范围。

### R2 仓库内部事实必须从源码核实

现有契约的边界（scheme、base_url 校验、Endpoint 形状、passthrough 判定、注册表装配）逐条给出
文件位置与推论，不靠印象。

### R3 外部事实诚实标注

候选本地推理服务器（Ollama / vLLM / llama.cpp / TGI）的 API 形状若无法访问官方文档，标为
**待核实假设**，不写成结论。可在仓库内核实的（如 `credential_ref` 是否必填）必须核实，不留作假设。

### R4 产出可验证的样例

给一份「本地模型服务器用现有 `kind: openai` 接入」的 YAML 样例，并用项目自己的
`Document::parse` 验证它能发布。这把 §3.1 从推测变成可检验的事实。

### R5 明确不做

- 不新增 `Provider` 实现、不改 `Endpoint`/`Destination`/`Scheme` 的签名。
- 不引入第二个传输栈。
- 不改 roadmap 之外的既有行为。

## Acceptance Criteria

- [ ] `research/findings.md` 覆盖三类异构，每类给出边界、改动面与"做/不做"的结论。
- [ ] 仓库内部事实逐条带 `file:line` 引用。
- [ ] 无法核实的外部事实明确列为假设，并列出验证方法。
- [ ] 本地模型服务器的 YAML 样例存在，且有测试证明它能被 `Document::parse` 解析。
- [ ] 决策建议写清对 roadmap 条目的具体修订（把「语义待澄清」替换成什么）。
- [ ] 产品代码零改动；`crates/**/*.rs` 只有新增的测试文件（若有）。

## Out of Scope

- 任何实现工作（新的 provider kind、新的 scheme、新的 trait）。
- 网络调研的补全（环境受限，列为后续动作）。
- Helm / registry / 部署形态（已由上一任务交付）。

## Open Questions

（无。三类边界由源码事实与 spec 的 second-consumer 规则决定；剩下的不确定性是外部事实，
已按 R3 标为假设。）
