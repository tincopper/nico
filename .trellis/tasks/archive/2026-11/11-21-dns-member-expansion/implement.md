# 执行计划：upstream 域名成员的 DNS 多成员展开

## 前置

- 需求与设计：同目录 `prd.md`、`design.md`；冲突以 PRD 验收标准为准。
- 约定：Rust 走 `.trellis/spec/backend/`；不提交（用户明确要求时才 commit）。

## 清单

### 1. nico-http：展开模块 `upstream/dns.rs`

- [ ] `ExpandedMembers`：嵌套 map（upstream 名 → endpoint 名 → 地址列表，`Arc<str>`），
      `Default`/`Clone`/`Debug`；缺省语义 = "map 里没有的 endpoint 按字面量单成员"。
- [ ] `MemberExpansionError`：`Unresolved { field, reason }`（category = `Upstream`：解析器这个
      依赖没答上来）与 `Collision { field, .. }`（category = `Client`：一份文档把一台机器写了两次）；
      `PartialEq`；字段形如 `upstreams[i].endpoints[j].address`。
- [ ] `expand_members_with(document, resolve)`：纯逻辑 + 注入解析；逐 endpoint——IP 字面量跳过
      （不在 map 里），域名解析 → 单名单内去重 → 地址集；每个 upstream 做完整碰撞检查
      （展开地址 vs 字面量成员、vs 其他展开成员的 `(address, port)`）；解析失败或空结果 →
      `Unresolved`。
- [ ] `expand_members(document)`：生产包装，`tokio::net::lookup_host` 收集**全部**地址。
- [ ] 单测（注入解析，无网络）：多地址展开、字面量跳过、失败字段名、空结果、碰撞两例
      （域名撞字面量成员；两个域名指同一台机器）、同名单内去重。

### 2. nico-http：成员构建接缝

- [ ] `member.rs`：`MemberId::new(name, address, port)`（`of(endpoint)` 保留给字面量路径）。
- [ ] `pools.rs`：`publish(&self, document, &ExpandedMembers)`；
      `UpstreamState::inherit(upstream, previous, expanded)`——有展开的 endpoint 逐地址建成员，
      没有的保持字面量单成员；`MembersView::names` 逐成员构建（同一 endpoint 名重复出现）；
      weight 继承 endpoint。
- [ ] `state.rs`：`publish_with_llm(snapshot, chains, llm, &ExpandedMembers)`；
      `state/tests.rs` 的调用点补 `&ExpandedMembers::default()`。

### 3. nico-server：发布管线

- [ ] `publish.rs`：`publish_state` 增参透传；`publish_candidate` 在 `config_compile_seconds`
      计时窗口**外**（DNS 是 I/O 不是 CPU）await `expand_members`，失败 →
      `refuse(&error, error.category(), "members", &version)`。
- [ ] `startup.rs`：`start_from_cache` 同样展开；失败 → warn（字段可读）+ `NotReady`——
      拒绝启动，无降级形态。

### 4. 测试

- [ ] pools：同名多 IP 是两个不同成员；同一 IP 跨发布继承健康；IP 消失随成员退出；
      字面量文档行为不回归。
- [ ] nico-server reload tests：`localhost` 成员的文档发布成功（真实 getaddrinfo 冒烟，
      对解析出 1 个还是 2 个地址不做假设）；`*.invalid` 成员 → `Rejected(Upstream)` 且
      LKG 保持服务。
- [ ] 既有 state / pools / upstream 测试全部不回归。

### 5. 文档与 spec

- [ ] README：`upstreams` 节写域名成员的展开语义；「当前限制」更新 DNS 条目。
- [ ] `docs/nginx-migration.md`：107 行（resolver/动态 DNS）与 251 行（动态 DNS）改为
      "发布期展开逐地址成员；TTL/权重解析仍不支持，DNS 变更靠 reload"。
- [ ] `.trellis/spec/backend/directory-structure.md`：`upstream/` 清单加 `dns.rs`，
      publish/expand 接缝与调用方向。
- [x] `.trellis/spec/backend/error-handling.md`：核对后确认发布拒绝阶段不在该 spec 逐条列出
      （阶段词汇由 `reload/publish.rs` 的 `refuse` 自述），错误分类规则
      （Unresolved=Upstream / Collision=Client）由错误类型自身的 `category()` 承载，
      不需要新的 spec 行。

### 6. 质量门

- [ ] `cargo +nightly fmt --all --check`。
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`。
- [ ] `cargo test --workspace --all-features --locked`。
- [ ] `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`。
- [ ] `cargo deny check`；feature matrix；文件预算（无 `.rs` > 500）。

## 复审门槛

- 展开逻辑无网络可测（注入解析）；仅 pipeline 冒烟走真实 DNS（`localhost` / `*.invalid`）。
- 请求路径 `upstream::resolve` 未改动：字面量短路保留，provider destination 的每请求解析
  是记录在案的边界（R4）。

## 回滚点

| 回滚到 | 方式 |
|---|---|
| 旧行为（每请求首个解析地址） | 文档把域名成员改写为 IP 字面量；无 schema/数据迁移 |
