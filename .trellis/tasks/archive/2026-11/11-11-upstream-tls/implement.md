# 实施计划：上游 TLS（rustls 后端）

> 顺序按"先把后端接上、再分层、最后验证"排。每步都有检查点；任一步检查点红了就停下修，
> 不带着红点进下一步。全部改动不提交（提交由用户决定）。

## 第 0 步：实现期复核（已完成，2026-11-11）

- [x] `ProxyServiceBuilder::build()` / `HttpProxy::new` 在 connector 构建失败时**会 panic**：
      `tls::Connector::new` 内部是 `build_connector(...).unwrap()`
      （`pingora-core/src/connectors/tls/rustls/mod.rs:50`）。→ bootstrap 必须用与 pingora 相同的
      解析路径（`rustls_pemfile::read_all` + `RootCertStore::add`）把一份坏 CA 文件拦在启动期。
- [x] `HttpSettings::resolve` 是唯一装配点（`crates/nico-http/src/settings.rs:64`，调用方
      `crates/nico-server/src/bootstrap/http.rs:35`），加字段只改一处及其调用方。
- [x] **实测补充（重要）**：未配置 `ca_file` 时，`cargo test -p nico-server --test ops_surface` 在
      `pingora-core/src/connectors/tls/rustls/mod.rs:50` panic，原因是 `load_native_certs()` 在本机
      返回 `errSecIO (-36)`。→ 新增第 3b 步（平台信任库启动期探针）与第 5 步的测试 harness 改造。
- [ ] `peer.options.ca`（`[WrappedX509]`）的构造成本探针——**只记录结论**，不进代码（每 provider
      一份 CA 不在本期）。**未做**：该事项属于后续里程碑，本期只到进程级 CA（见「落地记录」）。

## 第 1 步：打开 rustls 后端，删掉"没有后端"的分支

- [x] `crates/nico-http/Cargo.toml`：pingora features 改为 `["proxy", "rustls"]`，注释说明后端
      选择与它带来的构建前置（C 编译器）。
- [x] `upstream.rs`：删 `TLS_BACKEND_ENABLED`、`refuse_unavailable_tls`；`peer()` 只做
      `resolve` → `build`；模块注释里那条"noop_tls 是 pass-through"的段落改写为"后端已编译，见
      `tls` 子模块"。
- [x] `upstream/tests.rs`：删两条拒绝用例（`:79`、`:143`）、保留
       `the_host_header_keeps_a_non_default_https_port`（`:168`）与 `host_header` / `is_default_port`。

**检查点 A**：`cargo check -p nico-http --all-targets` 通过；`cargo tree -d | grep -E 'rustls|ring'`
只有一版；`cargo deny check licenses` 绿。

## 第 2 步：`upstream/tls` 两层

- [x] `tls/policy.rs`：`RootSource` / `TlsPolicy::from_bootstrap` / `root_source` / `validate_bundle`。
      **文件里不出现 `pingora` 标识符**。
- [x] `tls/adapter.rs`：`apply(&TlsPolicy, &mut ServerConf)`，唯一 import pingora TLS/配置文件类型
      的位置。
- [x] `tls/tests.rs`：`validate_bundle` 五种形状 + `RootSource` 映射 + adapter 写入 `ServerConf`
      的字段值。
- [x] `settings.rs`：`HttpSettings` 带 `TlsPolicy`（构造参数而不是 `Option<PathBuf>`，让类型表达
      "已经校验过"）。
- [x] `service.rs::server_conf()`：调用 `tls::adapter::apply`。

**检查点 B**：`cargo test -p nico-http --lib` 绿；`grep -n pingora crates/nico-http/src/upstream/tls/policy.rs`
无输出。

## 第 3 步：bootstrap 的 `tls` 段

- [x] `RawBootstrap` 增可选 `tls`；`tls` 出现但缺 `ca_file`、或路径为空 → 启动失败，字段名
      `tls.ca_file`。
- [x] 启动期读文件 → `validate_bundle`；不可读或不是有效信任根集合 → 启动失败，同样字段名。
- [x] 把校验后的 `TlsPolicy` 传进 `HttpSettings`。

**检查点 C**：四类 bootstrap 单测（省略整段 / 空段 / 不可读路径 / 合法文件）全绿；省略时行为与
今天完全一致。

## 第 3b 步：平台信任库的启动期探针（装配期）

- [x] `tls/mod.rs` 新增 `pub fn ensure_usable(policy: &TlsPolicy) -> Result<(), TrustRootsError>`：
      `Platform` → `rustls_native_certs::load_native_certs()`（与 pingora 同一个函数，尊重
      `SSL_CERT_FILE` / `SSL_CERT_DIR`）+ 至少一个证书；`Bundle(path)` → 重新 `fs::read` +
      `validate_bundle`。`nico-http` 增加 `rustls-native-certs` 直接依赖（0.7.3，与 `pingora-rustls` 同版）。
      “空集拒绝”的规则用一个吃 `&[CertificateDer]` 的私有函数做单测，不靠环境制造失败。
- [x] `crates/nico-server/src/server.rs::serve_with`：在 `http_service(...)` 之前调它；失败 →
      一条 `error!` + `ExitCode::from(FAILURE_EXIT_CODE)`，与 metrics 端口预检同形。
      **不放在 `BootstrapConfig::parse` 里**：parse 级单测不该被宿主平台信任库绑架。
- [x] 测试 harness：`crates/nico-server/tests/support/http.rs` 生成的 bootstrap 默认带
      `tls: { ca_file: <测试根 bundle> }`（新增 `crates/nico-server/tests/fixtures/tls/test-root.pem`），
      并覆盖其余自己拼 bootstrap 的集成测试（如 `metrics_startup.rs`）。

**检查点 D**：`cargo test -p nico-server --test ops_surface` 与
`cargo test -p nico-server --test metrics_startup` 在本机（不设 `SSL_CERT_FILE`）全绿。

## 第 4 步：错误映射

- [x] `proxy/failure.rs` 的超时谓词补 `TLSHandshakeTimedout`。
- [x] 单测：该 `ErrorType` 走超时分支；`TLSHandshakeFailure` / `InvalidCert` 走非超时分支（502）。

**检查点 D**：`cargo test -p nico-http --lib` 绿，且两条新用例分别断言 504 与 502 的分类。

## 第 5 步：真 TLS 上游 e2e

- [x] `crates/nico-server/tests/fixtures/tls/`：测试 CA（带 CA:TRUE）、叶子证书
      （`CN=localhost`，SAN 含 `localhost` / `127.0.0.1`）、另一张 SAN 只有 `example.invalid` 的叶子、
      两张叶子的私钥。用 `openssl` 一次性生成，有效期 100 年；目录里写一句“这些私钥仅用于测试”。
- [x] `nico-server` 的 **dev-dependencies** 加 `tokio-rustls`（起 TLS 监听器）。
- [x] `crates/nico-server/tests/tls_upstream.rs` + `tls_untrusted.rs`（各一个二进制、各一个真 server）：三个场景
      - 成功：bootstrap `tls.ca_file` 指向测试 CA → 2xx，上游收到请求路径与 Host；
      - 不受信：`ca_file` 换成另一张无关 CA（不是省略——省略会走平台库）→
        `502 upstream_unreachable`，cause 是证书校验失败；
      - 主机名不匹配：`ca_file` 指向测试 CA，但路由写 `127.0.0.1` 而证书只签了 `localhost` →
        `502`，cause 可区分。
- [x] 一条 `#[ignore]` 的公网冒烟（`tls_public.rs`，连 `https://example.com`），默认不跑。

**检查点 E**：三个用例稳定通过（连跑 10 次）；公网冒烟本机手动跑一次并记录结果。

## 第 6 步：文档与 spec 收口

- [x] `README.md`：改「当前限制」里的 TLS 条（只剩入站方向）；示例 `base_url` 本就是 `https://api.openai.com`；
      在配置章节补 `tls.ca_file` 的语义（**整体替换，不是追加**）。
- [x] `CONTRIBUTING.md`：工具表补 C 编译器（`ring` 的构建前置）。
- [x] `.trellis/spec/backend/pingora-guidelines.md`：错误矩阵两行与 noop_tls 那行改写为
      rustls 后端的现状；补一条"连接器在 service 构建时定型，CA 不热加载"。
- [x] `.trellis/spec/backend/directory-structure.md:201`：补 `upstream/tls/{policy,adapter}` 的两层结构
      与"policy 不含 pingora 类型"的约束。

**检查点 F**：`crates/nico-server/tests/structure.rs` 与文档相关断言全绿（该测试会抓 spec 举例里的
vendor 名，历史上抓过一次）。

## 第 7 步：全量门禁与自评

- [x] `cargo +nightly fmt --all -- --check`
- [x] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- [x] `cargo test --workspace --all-features --locked`（唯一红点是用户本地改的 `config/resources.yaml`，见「落地记录」）
- [x] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`
- [x] `cargo deny check`
- [x] 角色矩阵：`--no-default-features`、`-p nico-server --no-default-features --features traffic`
- [x] `cargo tree -d`：rustls / ring 各只有一版
- [x] 全树文件 ≤ 500 行
- [x] MSRV 决策落地：选“抬到 1.88”，`rust-version` 与 `clippy.toml` 的 `msrv` 同步，
      `CONTRIBUTING.md` 的工具表写出这条声明
- [x] 对照 `prd.md` 的验收清单逐条勾销，证据见「落地记录」

**回滚点**：任何一步发现"为 TLS 必须引入第二套 HTTP 栈"（违反设计 I5），停止实现回到设计。

## 落地记录（2026-11-11，第 3b/5/6/7 步）

### 实测命令与结论

| 命令 | 结果 |
|---|---|
| `cargo +nightly fmt --all -- --check` | 无输出 |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | 绿（`Finished dev profile`） |
| `cargo test --workspace --all-features --locked --no-fail-fast` | **66 个 suite 全绿，1 个用例红**：`nico-core::config::document::tests::llm::the_shipped_example_document_carries_a_converted_chat_route`。该用例断言 `config/resources.yaml` 里 `fast: gpt-4o-mini`，而该文件是用户本地的改动（`fast: auto/hy3`、`base_url: http://127.0.0.1:3425`），按本轮约束未触碰；与本任务改动无关，改动前即红。 |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | 绿 |
| `cargo deny check` | `advisories ok, bans ok, licenses ok, sources ok` |
| `cargo check --workspace --no-default-features` | 绿 |
| `cargo check -p nico-server --no-default-features --features traffic` | 绿 |
| `cargo test -p nico-server --no-default-features --features traffic --test role_traffic` | 3 passed |
| `cargo tree -d \| grep -E 'rustls\|ring\|webpki\|native-certs'` | 无输出（rustls 0.23.46、ring 0.17.14 各一版；`aws-lc-rs` 已从依赖图移除） |
| `find crates -name '*.rs' \| xargs wc -l \| awk '$1 > 500'` | 无输出 |

### 三个 e2e 场景的断言方式

`tls_upstream.rs`（`tls.ca_file = test-root.pem`）一个 server、三个场景：

1. **成功（upstream 路由）**：`scheme: https` + `host: localhost` → `200`，body 等于 origin 的固定应答；
   origin 收到的请求行是 `GET /tls-ok HTTP/1.1`，`host` 是 `localhost:<port>`。这个 200 只能来自配置的
   bundle（该证书不被任何平台库信任），所以它就是 `ca_file` 生效的证据。
2. **成功（provider 路由）**：`base_url: https://localhost:<port>` 的 `passthrough` 路由 → `200`；
   origin 收到 `POST /v1/chat/completions HTTP/1.1`、调用方 body 逐字节不变、`authorization` 是路由自己的
   凭证。证明 provider 端点走的是同一条 TLS 连接。
3. **主机名不匹配**：同一个 CA 签出的 `wrong-name` 叶子（SAN 仅 `example.invalid`），路由写 `127.0.0.1`
   → `502` + `x-nico-error-code: upstream_unreachable`；origin 侧观察到 `handshakes == 0`、
   `handshake_failures >= 1`、`requests.is_empty()`（即客户端完成了 TCP 连接但在校验阶段中止）；
   请求记录里 `error` 含 `not valid for name`。

`tls_untrusted.rs`（`tls.ca_file = other-root.pem`）一个 server、一个场景：origin 出示 `test-root` 签发的
叶子 → `502 upstream_unreachable`、响应体里不含 origin 应答的任何字节、origin 侧握手失败且无请求，
请求记录的 `error` 含 `UnknownIssuer`（与「主机名不匹配」的 cause 可区分）。

连跑 10 次：`tls_upstream` + `tls_untrusted` + `tls_startup` 每次都是 3 个 suite ok（10/10）。

### 启动期拒绝的实测（R9）

- 单测：`nico-http` 的 `tls::tests::probe`（空平台集拒绝、一个证书即可、bundle 重新读取、bundle 消失、
  bundle 不是信任根集合）。
- 装配期端到端：`tls_startup.rs` 两条——bootstrap 校验后被删除的 bundle、被换成私钥的 bundle，都是
  `ExitCode::from(FAILURE_EXIT_CODE)`，且日志里一行同时含真实原因与 `tls.ca_file`。
- 进程级实测（手工）：

  ```sh
  SSL_CERT_FILE=/nonexistent/roots.pem SSL_CERT_DIR=/nonexistent \
    ./target/debug/nico --bootstrap config/bootstrap.yaml
  # exit=1，一行 ERROR：
  # the upstream trust roots this process would use are not usable; set `tls.ca_file` …
  #   error=the platform certificate store could not be read: could not load certs from file
  #         /nonexistent/roots.pem: No such file or directory (os error 2)
  ```

  即平台库不可读时是**有名有姓的启动失败**，不再是 `HttpProxy::new` 里的 `unwrap()` panic。

### 公网冒烟

`cargo test -p nico-server --test tls_public -- --ignored`：**已跑，通过**（本机平台信任库可读，
`https://example.com` 返回 200）。该文件用 `write_bootstrap_on_platform_roots`（无 `tls` 段），
并在开头先断言 `ensure_usable(&TlsPolicy::default())`，这样在信任库不可读的机器上失败点会指向前提本身。

### 与计划/设计的偏离（均已在小范围确认）

1. **不受信场景独立成 `tls_untrusted.rs`**：`ca_file` 是进程级事实、connector 在 service 构建时定型，
   所以「另一个 CA」必须换一个进程；同一二进制里两个 server 就是测 Pingora 的全局量而非本 crate。
   三个场景仍是三个断言点，只是落成两个测试二进制。
2. **`ensure_usable` 返回 `TrustRootsError` 而不是 `BundleError`**：`BundleError` 描述的是「这堆字节不是
   信任根集合」，平台库不可读/为空放进它会让类型名说谎。`TrustRootsError` 是本轮新增的装配期错误
   （`PlatformUnreadable` / `PlatformEmpty` / `BundleUnreadable` / `Bundle(BundleError)`），
   形状与「一次装配期拒绝」一致。
3. **删掉了依赖图里的 `aws-lc-rs`**：上一轮把 `rustls` 写成 `=0.23.46`（默认 features），
   rustls 的默认 features 会打开 `aws-lc-rs`，于是同一个图里同时存在两个 crypto provider：
   `CryptoProvider::from_crate_features` 变得不明确（测试里 `ServerConfig::builder()` 直接 panic），
   并且多出一套 C 构建。现在声明为 `default-features = false, features = ["ring","logging","std","tls12"]`
   （与 `pingora-rustls` 请求的集合一致），`Cargo.lock` 里 `aws-lc-rs` / `aws-lc-sys` 消失。
4. **README「当前限制」的 TLS 条改写而不是整条删除**：入站 TLS 仍然不做（prd Out of Scope 要求 README
   继续「前面放代理」），原文把「数据面未启用 TLS」与「入站要代理」写在同一行，现在只剩后者。
5. **MSRV 抬到 1.88 的连带修改**：`clippy.toml` 的 `msrv` 一抬高，clippy 就开始建议 let-chain /
   `is_multiple_of` / `as_chunks`（这些正是 1.88 稳定的能力），于是全树有 24 处
   `collapsible_if` 等 lint 变红。用 `cargo clippy --fix` + `cargo +nightly fmt` 机械改写
   （`nico-core` / `nico-http` / `nico-llm` / `nico-plugins`，语义不变），再跑一遍门禁到绿。
6. **`resources.rs` 拆出 `resources/upstream.rs`**：加 `https` + `host` 之后该文件到 518 行，越过硬上限；
   按既有 `resources/route.rs` 的写法把 `UpstreamEntry` 及其渲染拆出去，回到 493 行。
7. `config/bootstrap.yaml` 追加一段**注释掉**的 `tls.ca_file` 示例（README 的配置章节已写同样的语义），
   `config/resources.yaml` 未触碰。

## 检查阶段修复（2026-11-11，独立复核）

复核结论：无 BLOCKER，可归档。以下是复核中就地修掉的项，未改设计取舍。

### 装配期探针与连接器对齐（唯一的行为修改）

1. **`require_platform_roots` 补上「每个条目可加载」这一半**（`crates/nico-http/src/upstream/tls/mod.rs`）。
   `pingora-rustls` 的 `load_platform_certs_incl_env_into_store` 是两步：`load_native_certs()`，再对每个
   条目 `RootCertStore::add(...)`；后者失败同样会在 `build_connector(...).unwrap()` 处 abort。原实现只查
   空集，因此「平台库可读、非空、但含一个不可解析条目」仍能走到那个 `unwrap()` —— 正是 §3.3 要关掉的
   失败面。现在探针按连接器的两步走，新增 `TrustRootsError::PlatformUnusable`（`#[source] rustls::Error`），
   `ensure_usable` 的 `# Errors` 同步，并新增单测
   `a_platform_entry_the_client_cannot_load_is_refused`；`one_platform_certificate_is_enough` 改用真实证书
   （原来用 `vec![1,2,3]` 断言「不过度解析」，正是被修掉的假设）。

### 测试与结构断言

2. **e2e 补一条「cause 不进响应体」断言**（`tls_untrusted.rs`）：除状态码与 `upstream_unreachable` 外，
   断言响应体不含 `certificate` / `UnknownIssuer`，把「cause 只进日志」变成可执行事实。
3. **新增结构测试** `the_tls_policy_layer_names_no_framework`（`crates/nico-server/tests/dependency_boundaries.rs`）：
   `include_str!` 读 `upstream/tls/policy.rs`，断言不含 `pingora` 且确实定义了 `RootSource` / `validate_bundle`
   （后者防止文件改名后断言变成空真）。这是 prd 验收里「用结构测试断言」那条的落地形式。

### 文档与一致性

4. `.trellis/spec/backend/pingora-guidelines.md`：错误矩阵的超时谓词行补 `TLSHandshakeTimedout`；证据表修正两处
   引用（`protocols/tls/rustls/client.rs:31-33` → `:31,37-38`；crypto provider 的 panic 实际发生在
   `get_default_or_install_from_crate_features` 的 `expect`，`from_crate_features` 只返回 `None`，引用改为
   `:243,249,265`）；`ensure_usable` 那行补「每个条目按连接器的方式 add」。
5. `.trellis/spec/backend/directory-structure.md`：`mod.rs` 行的职责补上「并按连接器的方式加载每个条目」。
6. `crates/nico-llm/src/provider/openai.rs`：删掉「this build cannot speak TLS」——该断言在本次改动后为假。
7. `.trellis/tasks/archive/2026-11/11-03-unified-gateway-analysis/implement.md`：把「当前构建拒绝所有 https」
   （还引用已删除的 `TLS_BACKEND_ENABLED`）改写为已由本任务关闭的事实。
8. `crates/nico-http/src/upstream/tls/tests.rs` 模块注释：原文说「no file system」，但 `mod probe` 的用例
   要读 bundle 路径，改为准确说法。
9. `crates/nico-http/src/upstream/tls/adapter.rs`：补一句「`display()` 对生产调用者无损，因为路径来自
   UTF-8 文档；将来的调用者若用 OS 字符串必须拒绝无法写成文本的路径」，把这个尖角写进契约。

### 依赖与格式

10. `tokio-rustls` 从 `crates/nico-server/Cargo.toml` 的直接版本移到根 `[workspace.dependencies]`，成员改为
    `workspace = true` —— 与本仓库「版本只在根声明一次」的既有形状一致；版本、`default-features = false`
    与 features 不变，`Cargo.lock` 未变。
11. `crates/nico-plugins/src/spec.rs`：`cargo clippy --fix` 之后遗留的一段结构体字面量缩进过深，手工对齐
    （`cargo +nightly fmt --check` 对这处不改写，属于 lint 工具的漏网）。

### 复核门禁（本机，2026-11-11）

| 命令 | 结果 |
|---|---|
| `cargo test -p nico-http --lib` | 312 passed（新增 1 条平台条目用例） |
| `cargo test -p nico-server --test tls_upstream --test tls_untrusted --test tls_startup` | 4 passed / 3 suites |
| `cargo test -p nico-server --test dependency_boundaries` | 4 passed（新增 1 条分层结构断言） |
| `cargo test -p nico-server --test ops_surface --test failed_pipeline --test metrics_startup` | 4 passed / 3 suites |
| `cargo +nightly fmt --all -- --check` | 无输出 |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | 绿 |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | 绿 |
| `cargo deny check` | `advisories ok, bans ok, licenses ok, sources ok` |
| `cargo check --workspace --no-default-features`、`-p nico-server --no-default-features --features traffic` | 绿 |
| `cargo tree -d`（rustls / ring）、`grep aws-lc Cargo.lock` | 各一版 / 无 `aws-lc-rs` |
| `find crates -name '*.rs' \| xargs wc -l \| awk '$1 > 500'` | 无输出 |

已知且与本任务无关：`nico-core` 的 `the_shipped_example_document_carries_a_converted_chat_route` 因
`config/resources.yaml` 的用户本地改动（`fast: auto/hy3`）而红，本轮未触碰该文件。
