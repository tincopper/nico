# 实施：异构 Provider 语义调研

本任务只交付调研与决策建议，无实现代码。

## 交付

- [x] `research/findings.md`：三类异构的边界、改动面与"做/不做"结论；仓库内部事实逐条带
      `file:line`；外部事实标为待核实假设。
- [x] `research/local-provider-example.yaml`：本地模型服务器用现有 `kind: openai` 接入的 YAML 样例。
- [x] `crates/nico-server/tests/local_provider_example.rs`（3 项）：证明样例能被 `Document::parse`
      解析、`kind` 是现有 openai、`model_map` 生效、`credential_ref` 必填这一事实被钉住。
- [x] `prd.md`：调研的边界与验收标准。
- [x] roadmap `11-03-unified-gateway-analysis/implement.md` §3/§4 修订：把「语义待澄清」替换为
      三类结论；同步把 usage / 部署形态标为已交付；明确只剩 token 限额。
- [x] README「本地模型服务器」小节：可复制的配置与两处注意事项。

## 验证

| 项 | 结果 |
|---|---|
| `cargo test -p nico-server --test local_provider_example` | 3 passed |
| 全树测试 / clippy / fmt | 见提交记录（产品代码零改动） |
| 外部假设（Ollama 等的 OpenAI 兼容 SSE 字节形状） | **未核实**，已明确列为假设并给出验证方法 |

## 明确未做

- 新的 `Provider` 实现、新的 scheme、新的传输栈（R5）。
- 网络调研的补全（环境受限）。
- token 限额（governance，需独立决策，见 roadmap §3）。
