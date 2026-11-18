# 方案 B 分析：纯对称双段（`http` / `https`），拆除 `http.tls`

> **状态：已被取代（superseded）**。本文的 D1-D4 缺陷分析仍是 `design.md` 的依据，但「顶层 `https` 段」的方案已被 `research/nginx-config-analysis.md` 的 `http.listeners[]` 取代。

> 用户倾向 B（"职责更明确"）。本文是该方案的完整后果分析，供决策；不改代码。

## 1. B 的语义模型

**一个段 = 一个 listener；段的存在即 listener 的存在。**

```yaml
# 纯明文
http:
  listen: 0.0.0.0:8080
  h2c: false

# TLS-only
https:
  listen: 0.0.0.0:8443
  cert_file: ...
  key_file: ...

# 双 listener
http: {...}
https: {...}
```

- `https` 段：`listen` 必填、`cert_file`/`key_file` 必填、`deny_unknown_fields`；空段 = 错误。
- 不再有"接管"概念：没有哪一段会翻转另一段的含义。

## 2. 必须先解决的四个设计问题

### D1：进程旋钮的归属（B 的核心缺陷）

`http` 段今天装着三类事实：listener 事实（`listen`、`h2c`）、**进程**事实（`grace_period_ms`、`graceful_shutdown_ms`、`threads`）。B 下 `http` 段缺席 = 无明文 listener，TLS-only 部署将**无法配置** drain 窗口——而终止 TLS 的 AI 网关恰恰最需要调它（长流式响应）。

| 子选项 | 内容 | 代价 |
|---|---|---|
| **B-a（推荐）** | 旋钮上移到新顶层段 `server: { threads, grace_period_ms, graceful_shutdown_ms }` | 多破一次配置形状（3 个字段搬家）；但一次到位 |
| B-c | 旋钮留 `http`；段缺席 → 默认值 | 破坏面最小；TLS-only 旋钮不可调；将来还得二次破坏 |

推荐 **B-a**：B 本来就要破两次兼容（拆 `http.tls` + 改 `http` 缺席语义，见 D2），旋钮一起归位只多一次搬家；否则 `http` 段名不副实地装着进程旋钮，违反"职责明确"的初衷。名字 `server` 可商榷（`runtime` 亦可），职责是"进程运行时旋钮"。

### D2：`http` 段缺席的语义变化（今天已验证的破坏点）

今天（bootstrap.rs:361-364）：`http` 段**缺席** = `RawHttp::default().resolve()` → 明文 listener 绑 `0.0.0.0:8080`（`DEFAULT_LISTEN`，settings.rs:17）。

B 下：**段缺席 = 无该 listener**。`https` 缺席同理。两条段都缺席 → 启动拒绝（无任何 listener）。

- 段存在但 `listen` 缺省 → 沿用 `DEFAULT_LISTEN`（`http: { h2c: true }` 这类写法继续有意义）。
- 破坏性：写全段的部署（示例配置如此）无感；裸奔部署（零配置起 8080）会改为启动失败。这是把"隐式默认"换成"显式声明"，与"空 `http.tls:` 是错误不是未启用"的既有决策同方向。

### D3：metrics 碰撞检查耦合了 data plane 地址

`metrics.resolve(http.listen())`（bootstrap/metrics.rs:47）用明文 listener 地址做端口碰撞校验，且 `BootstrapConfig` 组装时以 `http.listen()` 为输入。B 下：

- 双 listener：碰撞校验必须对**实际要绑定的地址集合**（明文 + TLS）进行；
- TLS-only：`http.listen()` 不存在，签名必须改（`resolve(&self, listeners: &[String])` 之类）。

不是难点，但是必改清单上的一行，写进设计免得漏。

### D4：h2c 归属

留 `http`（明文 listener 事实）。TLS-only 时 `http` 段缺席，`h2c` 无处可写——语义自然闭合，无需新校验。双 listener 下 h2c 作用于明文口。今天的"h2c + TLS 冲突"校验**整条删除**（没有接管模式了）。

## 3. B 之后的校验矩阵

| 配置 | 结果 |
|---|---|
| `http` 与 `https` 都缺席 | 启动拒绝（无 listener） |
| `https.listen` 与 `http.listen` 解析后相同 | 启动拒绝，字段 `https.listen` |
| `https` 空段 / 缺 `cert_file`/`key_file` | 启动拒绝，对应字段（现规则平移） |
| 坏证书/坏 key/不匹配 | 启动拒绝（`validate_server_identity` 路径复用，不变） |
| `http.listen` 或 `https.listen` 端口 0 | 启动拒绝（现规则） |
| `h2c` 与 `https` 并存 | 允许（不同 listener） |

`x-forwarded-proto` 不受本决策影响：按连接 `ssl_digest` 判定（决策 3 原样保留），三种部署形态下都正确。

## 4. 重构影响面（拆除 `http.tls` + B-a 旋钮搬家）

| 文件 | 改动 |
|---|---|
| `nico-http/settings.rs` | `HttpSettings` 拆形：明文 listener 字段 + `tls_listener: Option<TlsListener>` + 旋钮迁出（B-a）；`resolve*` 签名重造；`DEFAULT_LISTEN` 语义随 D2 调整 |
| `nico-http/service.rs` | 三分支注册（明文 / TLS / 双）；`server_conf(&settings)` 输入源随 B-a 变 |
| `nico-http/proxy.rs`、`hooks/request.rs` | 删 `inbound_tls` 标志，改按连接 `ssl_digest`（原计划不变） |
| `nico-http/inbound.rs` | `InboundTls` 类型复用不动 |
| `nico-server/bootstrap.rs` | 顶层 `https` 段；`http`/`https` 至少其一；`server` 段（B-a）；metrics 校验签名（D3） |
| `nico-server/bootstrap/http.rs` | `RawHttp` 删 `tls` 字段，变纯明文段 |
| `nico-server/bootstrap/https.rs` | **新增**：解析 + 读文件 + `validate_server_identity` → `InvalidField("https.*")` |
| `nico-server/bootstrap/metrics.rs` | 碰撞校验改对地址集合（D3） |
| `bootstrap/tests/http_tls.rs` | 重写：https 段场景 + D2 语义 + 双 listener；h2c 冲突测试删除 |
| `tests/support/http.rs::write_bootstrap_tls` | 改写 `https:` 段 |
| `tests/tls_inbound.rs` | 接管 e2e → TLS-only e2e + 双 listener e2e |
| spec 三件 + `config/bootstrap.yaml` + README | 契约表、错误矩阵、示例重写 |

规模：约等于半天到一个单元的重构，全部落在**昨天刚提交、未发布**的面上。

## 5. 兼容性论证（为什么现在切是对的）

- `http.tls` 与"缺席 http = 默认 8080"都是本周期刚交付、未发版的形状；pre-1.0 的配置面就是 API，现在改是最后便宜的窗口。
- 若先按 C（保留 `http.tls` + 新增 `https`）交付，"两种 TLS 拼法 + 互斥规则"将进入文档与用户心智，将来收敛成 B 时破坏面只会更大。
- 项目既有决策方向支持显式化："空 `http.tls:` 是错误不是未启用"、"一个 listener 一种传输"。

## 6. 结论

B 成立，且比 C 更符合"职责明确"的目标，但**必须连 D1 一起做**（旋钮上移），否则留下 TLS-only 不可调 drain 的运维缺陷。D2/D3/D4 是连带必改项，不是可选。

推荐组合：**B + B-a（`server` 段）**。待确认后重写 `prd.md` 与 `design.md`。
