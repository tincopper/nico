# 上游 TLS：rustls 后端、TLS 策略层与真 TLS 上游验证

## Goal

让数据面能真的连上公网 https 上游。今天所有 `https` 目标在连接前被拒绝
（`crates/nico-http/src/upstream.rs:47` `TLS_BACKEND_ENABLED = false`），因此 LLM 协议转换与普通转发
在真实世界都不可用——全部既存证据都来自明文 mock。本任务打开 pingora 的 rustls 后端，把 TLS 策略
与 pingora 适配器分层，并用真 TLS 上游证明成功与失败两条路径。

## Background

- 后端选型已定：**只用 rustls**。boringssl 不做；openssl（nginx 那条路）留到以后，本任务不为它留
  feature 骨架。
- 取证结论见 `research/rustls-backend-facts.md`：rustls 0.23.12 + ring，TLS 1.2/1.3，平台根证书
  （尊重 `SSL_CERT_FILE` / `SSL_CERT_DIR`），`ConnectorOptions.ca_file` 与 `PeerOptions.ca` 都是
  "替换"根集而非追加，peer 级 `verify_cert` / `verify_hostname` 真正生效。
- 连接器在**构建 service 时**生成，读的是 `ServerConf.ca_file`（`pingora-proxy-0.9.1/src/lib.rs:222`）；
  nico 的装配顺序是先替换 `server.configuration` 再建 service（`crates/nico-server/src/server.rs:105,154`），
  所以进程级 CA 走 `ServerConf` 是可行的，但**启动后不可热改**。
- 现有超时谓词只认 `ReadTimedout | WriteTimedout | ConnectTimedout`
  （`crates/nico-http/src/proxy/failure.rs:94`），`TLSHandshakeTimedout` 会被当成普通连接失败。

## Requirements

### R1 数据面可以连 https 上游

- `nico-http` 启用 pingora 的 `rustls` 后端；`providers[].base_url: https://…` 与
  `upstreams[].scheme: https` 都不再在连接前被拒绝。
- 默认**开启**证书校验与主机名校验（`PeerOptions` 默认值即可，不额外关掉任何一项）。
- SNI 取目标 host，与今天的 `HttpPeer::new(addr, tls, host)` 一致。

### R2 根证书来源：平台库默认，bootstrap 可整体替换

- 未配置时使用平台证书库（`rustls-native-certs`）。`SSL_CERT_FILE` / `SSL_CERT_DIR` 由 provider
  自身读取，属部署手段，不进 nico 的配置面。
- bootstrap 新增可选 `tls` 段，唯一字段 `ca_file`（路径）。给出时**整体替换**平台根集——这是
  pingora 的语义，不是本项目的选择，必须在文档里写清："这是完整信任根集合，不是追加"。
- `ca_file` 在启动时读取一次；路径不存在或内容不是 PEM 证书 → 启动失败并指名 `tls.ca_file`，
  不静默退回平台库。
- 明确不做热加载：reload 只换文档快照，不重建 service。改了 CA 必须重启。

### R3 策略与适配器分层，为以后的抽取留缝

`crates/nico-http/src/upstream/tls/` 拆成两层：

- `policy.rs`：**不含任何 pingora 类型**。描述"信任什么、校验什么"（根集来源、verify 开关、未来的
  client cert / 版本范围），只有一个职责：把配置事实变成可断言的策略值。
- `adapter.rs`：把 policy 映射成 pingora 的 `ConnectorOptions` / `PeerOptions`。今天唯一的消费者。

`upstream.rs` 的 `build()` 只负责组装，不再承载策略判断。抽取成 `nico-tls` 的触发条件写在
`design.md` 的 Reserved seam 一节，本任务**不建**新 crate。

### R4 错误映射

- 握手失败、证书不受信、主机名不匹配 → `502 upstream_unreachable`，类型化 cause 只进日志
  （沿用现有单一出口与"响应体不泄露内部细节"的规则）。
- `TLSHandshakeTimedout` 归入超时谓词 → `504 upstream_timeout`，与其它超时同一条路径。
- 不新增对外的 `x-nico-error-code`。

### R5 删除"没有 TLS 后端"的分支

- 删掉 `TLS_BACKEND_ENABLED`、`refuse_unavailable_tls` 及其 2 条测试（`upstream/tests.rs:79`、`:143`；
  `:168` 是 `the_host_header_keeps_a_non_default_https_port`，与 TLS 无关，保留）。
  该测试的原意是"宁可失败也不明文降级"；后端编译进来之后这个状态不可表示，测试对象不存在。
- 由 R6 的"不受信 CA 必须失败"测试承接同一意图：失败必须是显式的，不是降级。

### R9 平台根证书在启动期校验（实测新增）

- 实测：无 `tls.ca_file` 时，`ProxyServiceBuilder::build()` 会在
  `pingora-core/src/connectors/tls/rustls/mod.rs:50` panic（`load_native_certs` 返回 `errSecIO (-36)`）。
  即默认路径在无法读平台信任库的机器上会在建 service 时崩溃，而不是报启动错误。
- 因此未配置 `ca_file` 时也要探一次平台信任库（`rustls-native-certs 0.7.3` 的
  `load_native_certs()`，与 pingora 同一个函数、同样尊重 `SSL_CERT_FILE` / `SSL_CERT_DIR`）：
  不可读或为空 → 启动失败，并提示设置 `tls.ca_file`。
- 校验发生在**装配期**（`serve_with`，与 metrics 端口预检同一位置）而不是 bootstrap 解析期：
  解析期只校验写出来的 `ca_file`；装配期统一确认“connector 将要读到的就是可用的”（平台库或
  重新读取的 bundle 文件）。这样 parse 级单测不会被平台环境绑架。
- 规则与 bundle 一致：至少一个证书。空的平台集 = 信任任何东西，必须在启动期拒绝。

### R6 用真 TLS 上游验证

- 本地自签 CA + 真 TLS 监听器的 e2e，三个场景：成功、不受信 CA 失败、主机名不匹配失败。
- 一条 `#[ignore]` 的真实公网冒烟（无凭证依赖）：连一个公网 https 站点，断言完成握手并拿到响应。
  CI 与无网环境默认不跑。

### R7 依赖、许可与构建前置

- 新依赖只有 pingora 的 rustls 后端（`pingora-rustls` / `rustls` / `ring` / `rustls-native-certs` /
  `rustls-pemfile` / `tokio-rustls` / `x509-parser` / `ouroboros`）。
- `cargo deny check` 全绿：`ring 0.17.14` 的 `Apache-2.0 AND ISC` 已在允许列表内，实测确认。
- `cargo tree -d` 不出现第二套 TLS/crypto 栈（rustls 只有 0.23 一版）。
- `ring` 含 C/汇编，构建需要 C 编译器。`CONTRIBUTING.md` 的工具表要补这一条（不需要 cmake/ninja）。

### R8 文档收口

- `README.md`「当前限制」删除 TLS 那条，示例配置的 `base_url` 回到 `https://api.openai.com`。
- `.trellis/spec/backend/pingora-guidelines.md:255`（`https` → 502 那张表）与 `:388`（noop_tls 是
  pass-through）改写为"rustls 后端已编译，https 正常握手；失败分类见 R4"。
- `directory-structure.md:201` 保持 `nico-http` 拥有 upstream TLS policy，并补上 `upstream/tls/` 的
  两层结构。

## Out of Scope

- 入站 TLS 终止（数据面 `listen` 上的证书服务）；README 继续要求前面放代理。
- mTLS 配置面（`client_cert_key`）；`peer.options.ca` 的 per-provider CA。
- 上游 HTTP/2 / ALPN H2（`PeerOptions.alpn` 保持 `H1`）。
- openssl / boringssl / s2n 任一后端，以及为它们预留的 feature 骨架。
- 证书热加载与轮换、OCSP / CRL（pingora 的 rustls 路径自身仍标 TODO）。
- QUIC / HTTP/3。

## Acceptance Criteria

- [ ] `upstreams[].scheme: https` 的转发与 `providers[].base_url: https://…` 的 LLM 路由在真 TLS
      上游上完成握手并返回响应；`refuse_unavailable_tls` 不再存在（`cargo doc` 与源码里都搜不到）。
- [ ] e2e 三场景（成功 / 不受信 CA / 主机名不匹配）各自有断言：成功得 2xx 且上游收到请求；不受信
      得 `502 upstream_unreachable`，日志里 cause 是证书校验失败；主机名不匹配同样 502 且可区分。
- [ ] `TLSHandshakeTimedout` 走 504：一条单测覆盖 `proxy/failure.rs` 的谓词，一条断言对外 code 是
      `upstream_timeout`。
- [ ] bootstrap `tls.ca_file` 的行为：路径不可读、内容不是信任根集合、段存在但字段缺失 → 三种都启动
      失败且错误字段名是 `tls.ca_file`；合法时断言它被写进 `ServerConf`（“整体替换而非追加”是 pingora
      的语义，已在 spec 记录并给源码位置）。
- [ ] `upstream/tls/policy.rs` 里没有任何 `pingora` 标识符（用 `cargo doc` 的依赖视图或一条结构测试
      断言）；`adapter.rs` 是唯一 import pingora TLS 类型的地方。
- [ ] 平台信任库不可用时**启动失败而不是 panic**：未配置 `tls.ca_file` 且平台库读不到/为空 →
      返回 `BootstrapError`，进程以 `FAILURE_EXIT_CODE` 退出，消息提示设置 `tls.ca_file`。
- [ ] 真公网冒烟：`cargo test -p nico-http --test <file> -- --ignored` 在本机通过（记录结果，不作为
      门禁）。
- [ ] 五条基线全绿：`cargo +nightly fmt --all --check`、`clippy --workspace --all-targets
      --all-features -D warnings`、`cargo test --workspace --all-features --locked`、
      `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps`、`cargo deny check`。
- [ ] 角色矩阵：`--no-default-features` 与 `--features traffic` 都能编译，且 traffic 角色同样连得上
      https 上游（TLS 不进 feature 门）。
- [ ] 全树文件 ≤ 500 行；`cargo tree -d` 无 rustls / ring 的第二个版本。

## Open Questions

- **MSRV**：advisory-free 的 `time 0.3.55`（经 `x509-parser → asn1-rs` 进入生产依赖树）声明
  `rust-version = 1.88`，而工作区写的是 `1.85`。当前 `rust-toolchain.toml` 钉 1.99，构建不受影响，
  但声明是假的。需选择：把工作区 `rust-version` 抬到 1.88（推荐，让声明成真），或保留 1.85 并在
  spec 里记录这一偏差。
