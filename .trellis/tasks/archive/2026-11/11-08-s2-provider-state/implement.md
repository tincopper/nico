# 阶段 2 可靠性核心：实施计划

> 设计见 `design.md`。顺序是硬约束：Pingora 会话重建取证（第 1 步）通过之前不写尝试循环的代码；
> 配置与发布期编译（第 2 步）是 3/4 的前置；数据面循环（第 5 步）依赖 1 的结论与 4 的判定逻辑。
> 每步保持工作区绿：`cargo +nightly fmt --all -- --check`、`cargo clippy --workspace --all-targets
> --all-features --locked -- -D warnings`、`cargo test --workspace --all-features --locked`。

## 0. 规模

- 新增/修改约 20 个源文件、约 3k 行（含测试），步骤 1–8。
- 依赖链：`1 → {2 → 3 → 4 → 5, 6} → 7 → 8`。第 6 步（role 构建）与 2–5 无依赖，可并行。

## 1. 研究检查点（先做）：Pingora 会话重建取证

- [x] 回答：在 `proxy` 任务内、**客户端未收到任何字节**之前，能否拆掉当前 upstream 会话、
      按 `AttemptPlan` 换目的地重发请求头与 body？给出可复用的回调组合与最小探针代码。
- [x] 必须覆盖：换目的地（fallback 目标可能是不同 host/port）时连接池与 `HttpPeer` 的重建；
      请求 body 的二次写出（缓冲 body 是否可从 `HttpContext` 重取）；`fail_to_connect` 与响应头后
      放弃会话各自的发生时机；graceful/取消语义在循环中的表现。
- [x] 实证不到的（例如某回调组合无法安全重入）一律落成设计修订，不在循环里打补丁。
- [x] **检查点 R**：`research/pingora-retry.md` 存在，写明选定的挂点、探针证据与风险；设计 §2 的循环
      按结论修订（如需）。

  **落地记录（第 1 步）**
  - 结论：0.9.1 的 `process_request` 自带 `while retries < max_retries` 尝试循环，每轮重新调用
    `upstream_peer` 并重建 upstream 会话；请求头每轮从下游请求重新克隆并重跑
    `upstream_request_filter`，所以换 host/port 与逐尝试改写头是框架原生能力。挂点因此定为
    **框架循环 + `ServerConf::max_retries` 上界**，不在 `proxy` 任务里自造第二条循环。
  - 硬边界：body 重放走内建 retry buffer（`BODY_BUF_LIMIT = 64 KiB`，不可配）。body 已读完且
    raw body 超限时截断，下一轮 `get_retry_buffer()` 返回 `None` 且 `request_body_filter` 不再被调用
    ——重试轮会发出无 body 的请求。因此「分派后重试」只在 raw body ≤ 64 KiB 时合法，connect
    阶段失败不受此限。
  - 证据形态：本机 registry 源码静态精读（`pingora-proxy`/`pingora-core`/`pingora-error` 的文件:行号），
    未做运行时探针（理由写在 `research/pingora-retry.md` §5）。
  - 该边界落成设计修订而非循环补丁：`design.md` §1 失败模式表、§2、§8 测试策略各有一条，e2e
    「> 64 KiB body 分派后失败不重试」是它的证据（第 7 步）。

## 2. 配置与发布期编译

- [x] `nico-core`：schema 三件（`routes[].retry_budget`、`routes[].fallback`、
      `providers[].circuit_breaker`）+ raw/semantic/validate 四段校验 + `document` 测试；
      链长上限与预算上限的常量入文档模型。
- [x] `nico-server/llm/plan.rs`：路由编译出 `AttemptChain`——逐成员 endpoint/model_map/凭证物化、
      能力交集、目标身份指纹；任一成员 RouteFact → 整条路由发布失败（LKG 保活）。
- [x] **检查点 A**：发布期拒绝有测试（未知成员、重复成员、坏阈值、成员 RouteFact）；未声明
      fallback/retry 的路由编译结果与现状逐字节一致（零行为变化的证明）。

  **落地记录（第 2 步）**
  - `nico-core`：`routes[].retry_budget` 默认 1、上限 `MAX_RETRY_BUDGET = 4`；`routes[].fallback`
      上限 `MAX_FALLBACK_MEMBERS = 3`；`providers[].circuit_breaker` 的 `failure_threshold` 默认 5、
      `cooldown_ms` 默认 30 000，阈值 `0` 只计数不拒绝。`MAX_ATTEMPTS_PER_ROUTE =
      MAX_RETRY_BUDGET × (1 + MAX_FALLBACK_MEMBERS)` 由两个常量相乘得出，数据面直接读它当
      `ServerConf::max_retries` —— 框架计数与文档模型是同一个数，不是两个要保持一致的数字。
  - 校验：raw → semantic → validate 四段；两个新字段在 `passthrough` 路由上被拒（无 canonical 值、
    无失败分类，没有可重试的东西）；链成员重复、未知 provider、未知成员都在文档层先拒。
  - 测试：`document/tests/attempts.rs`、`document/tests/providers.rs`、`semantic/tests/{attempts,chain,providers}.rs`、
    `reference/tests/fallback.rs` 覆盖默认值、上限、passthrough 拒绝与引用解析。
  - `nico-server/llm/plan/chain.rs`：逐成员编 endpoint/model_map/凭证（同一 provider 只读一次），
    能力取全体交集，每个成员编出自己的 `TargetIdentity`（provider 名 + base URL + kind + breaker
    参数）——目标身份在发布期算完，请求期不再推导。任一成员不可服务 → 整条路由发布失败，LKG 保活。
  - 落点：`plan/facts.rs` 抽走路由事实，`plan.rs` 保持在 300–500 行预算内。
  - 检查点 A 的证据：`tests/plan/chain.rs` 五个拒绝用例（未知成员、kind 无适配器、凭证不可读、
    passthrough + fallback、passthrough + retry_budget）；零行为变化由
    `a_route_without_attempts_compiles_one_target`（未声明 attempts 的路由仍编成单目标链）钉住。

## 3. 熔断状态机与 SharedServices

- [x] `nico-llm`：目标级状态机（closed/open/half-open、failure_threshold/cooldown、单探测）；
      状态表按身份指纹继承（配置未变跨 generation 保留）。
- [x] `nico-server`：`SharedServices` 创建一次、`Arc` 进发布链路与快照；不做通用 Lifecycle Manager。
- [x] **检查点 B**：状态机全迁移单测 + 继承/重置测试；`SharedServices` 的生命周期（进程启动/关闭）有测试。

  **落地记录（第 3 步）**
  - `nico-llm/src/breaker.rs`：`closed → open`（连续失败达阈值）→ `half_open`（冷却到期后只放行
    一个探测）；探测成功关闭、失败立即重开；open 期间到达的失败不延长冷却；`retain` 在文档不再
    点名某目标时丢弃它的状态。
  - `nico-server/src/llm/shared.rs`：`SharedServices` 是进程级持有者，本期只承载 breaker table，
    明确不是通用 Lifecycle Manager（第二种长寿命状态到来时加自己的字段，规则相同：文档仍点名才继承）。
    `llm::runtime` 持 `Arc<SharedServices>`，`with_shared` 是世代继承的注入缝；发布链路编译链、
    请求路径判定与更新都引用同一张表。
  - 测试：`breaker/tests.rs` 覆盖全迁移（阈值、成功复位、冷却到期单探测、探测成功/失败、探测丢失后
    再等一个冷却、open 期间失败、关闭态只计数不拒绝、身份继承、参数变更重置、`retain` 清理）；
    `shared.rs` 两条（一个实例服务进程内所有句柄、文档删掉目标即丢状态）；
    `tests/runtime.rs::a_generation_inherits_the_history_of_the_targets_it_kept_and_drops_the_rest`
    用同一张表编译两代，断言留下者继承、移除者清零。

## 4. 目标选择与判定（nico-llm）

- [x] `classify`：provider 状态 → canonical 错误 → {Serve, Attempt, Exhausted}；retryable 沿用闭表。
- [x] 次序逻辑：预算内重试同一目标 → 计熔断 → 切下一目标 → 链尾 Exhausted；熔断 open 的目标不产生
      AttemptPlan（记 skipped）。
- [x] **检查点 C**：exchange 级 mock adapter 序列测试（fail→fail→ok、401 即停、发头后不进入判定、
      canonical 只解码一次而逐目标 wire 不同）。

  **落地记录（第 4 步）**
  - `nico-llm/src/route/attempt.rs`：一次解码 canonical，按目标逐次编码；`Verdict::{Answer, Exhausted,
    Retry}` 把「当前答复就是答案」「链走完」「还有下一次尝试」做成三个返回值，调用方没有第四种选择。
  - 次序：预算内先重试**同一目标**（网络抖动会自愈，且不引入新端点的不确定性）→ 预算耗尽才换下一个
    成员 → 链尾 `Exhausted`。离开一个目标只记**一次** breaker 失败（不是每次尝试一次）。open 的目标
    不产生 plan（记 skipped）；全部被跳过时用最后记录的错误应答（`SelectionRefused`）。
  - 能力交集检查在任何目标之前：请求需求超出链交集 → 422 `unsupported_capability`，一个字节都不发。
  - 测试：`tests/route/attempt.rs` 覆盖预算内重试同目标、预算耗尽换成员、401 立即应答、open 目标被跳过、
    全跳过用最后错误、能力拒绝在目标之前、只解码一次而逐目标 wire 不同、传输失败与 provider 502 同轨。
    「发头后不进入判定」属于数据面事实，见第 5 步的 `a_committed_response_never_replaces_an_attempt`
    与第 7 步的 e2e。

## 5. 数据面尝试循环（nico-http）

- [x] 字节缝扩展：`AttemptPlan` 的传递与"上一次尝试的结果"问询（保持 `nico-http` 无厂商名、不依赖
      `nico-llm`）；`HttpContext` 的 LLM 状态持有各 attempt 编码（≤ max_body_bytes × 链长，发布期已限）。
- [x] 尝试循环按第 1 步选定的挂点落地；响应头写出后循环必终止（不变量 2 有断言）。
- [x] 每尝试日志接插件（`llm.attempt`/`llm.target`/结果/耗时）。
- [x] **检查点 D**：hook 级测试（无 socket）：循环次序、发头后终止、passthrough 路由零进入。

  **落地记录（第 5 步）**
  - `context/attempt.rs`：LLM 状态持有这次请求的走链者（`dyn LlmAttempt`），同一时刻只有一份编码
    在途（`prepare` 返回的 `LlmPrepared`），最坏内存由发布期已限的链长与 body 上限决定。
    `begin_attempt_body` 只由每轮跑一次的 hook 调用，客户端字节因此只计一次；`may_replace_attempt
    (committed, FailureStage)` 是重试合法性的唯一门——未声明 attempts、已提交、客户端已离开、超过重放
    上限，四种情况都在同一处拒绝。
  - `proxy/hooks/llm.rs::retry_admitted` 是那扇门唯一的调用者；`request.rs`（分派前）、`response.rs`
    （响应头翻译失败，以及框架自己上报的上游错误经 `error_while_proxy`）、`fail_to_connect`（连接失败，
    分派前）三处都经由它；`fail_to_proxy` 是失败答复唯一的写者。
  - 日志：每次尝试一行 `llm.target`/`llm.attempt`/`llm.outcome`（`failed`/`skipped`），breaker 相位变化
    一行 `llm.circuit`（`closed`/`open`/`half_open`）；都不带 body、不带凭证。
  - `service.rs`：`ServerConf::max_retries = MAX_ATTEMPTS_PER_ROUTE`，框架的循环计数器与文档模型共用
    一个常量（`the_attempt_ceiling_covers_every_chain_a_document_can_declare` 钉住）。
  - 检查点 D 的证据分布：循环次序在 `tests/route/attempt.rs`（无 socket）；未声明 attempts 的路由零
    进入由 `context/tests/attempt.rs::a_route_that_declared_no_attempts_never_replaces_one` 与
    `proxy/tests/llm/request.rs::an_upstream_route_leaves_the_request_alone` 钉住；发头后终止由
    `a_committed_response_never_replaces_an_attempt` 与第 7 步的 e2e 场景 4 钉住。

## 6. traffic/llm role 构建（可与 2–5 并行）

- [x] `nico-server` feature `llm`（默认开）；`--no-default-features --features traffic` 不编入
      `nico-llm`；`cfg` 门控 llm 模块与注册装配。
- [x] 角色合法性沿用既有发布校验（空注册表拒绝 provider 路由，LKG 保活）——补一条 e2e 或 plan 测试
      锁定 traffic 形态的行为。
- [x] **检查点 E**：`cargo check --no-default-features --workspace`、`--features traffic`、
      `--features llm`、`--all-features` 四种组合通过。

  **落地记录（第 6 步）**
  - `nico-server/Cargo.toml`：`default = ["llm"]`、`llm = ["dep:nico-llm"]`、`traffic = []`。角色是产物
    的属性，不是 bootstrap 文件能填错的字段；traffic 构建不编入 `nico-llm`，注册表因此为空。
  - llm 模块与注册装配用 `cfg(feature = "llm")` 门控；traffic 角色遇到 `routes[].provider` 时由既有
    发布校验拒绝整份文档，旧 generation 继续服务——不是静默跳过。
  - `tests/role_traffic.rs`（`#![cfg(not(feature = "llm"))]`）三条：无 provider 路由的文档可编译、
    带 provider 路由的文档发布失败、traffic plan 不服务任何路由。因为 `--all-features` 会把它们编译成
    空，`scripts/gates/pre-push.sh` 专门跑一次
    `cargo test -p nico-server --no-default-features --features traffic --test role_traffic`。
  - 检查点 E 在收尾门复跑：四种 feature 组合 `cargo check` 全通过（见第 8 步门记录）。

## 7. 端到端

- [x] `nico-server/tests/llm_fallback.rs`：A 502→B 200（缓冲与流式各一）、A 持续失败熔断后 B 服务且
      A 连接数为 0、401 不重试、发头后死亡不换源、重试期间逐尝试日志无 body 无凭证。
- [x] **检查点 F**：AC 的前四条逐项有 e2e 证据。

  **落地记录（第 7 步）**
  - `tests/llm_fallback.rs` 一个 server、七个场景，全部断言在 wire 上（哪个 origin 被连接、几次、调用方
    看到了什么）：502 → fallback（缓冲）、熔断打开后该目标连接数为 0、401 不 fallback、发头后死亡不
    换源、> 64 KiB body 分派后不重试、目标解析失败被走过、流式答复由 fallback 供给。
  - 重试的字节事实：同一次 502 请求下 primary 恰好两次连接（budget 2），且两次上游请求体**逐字节相同**
    ——重试从 canonical 重新编码，不是重放客户端字节。
  - 日志断言：`llm.outcome="failed"` + `llm.target=primary`、`llm.circuit="open"` + `llm.target=primary`、
    跳过目标的那一行；请求记录里既无凭证也无 prompt。

## 8. spec 与收口

- [x] `error-handling.md`：重试/熔断/fallback 的判定表与"发头即单行门"；`pingora-guidelines.md`：
      尝试循环挂点与在途状态；`logging-guidelines.md`：`llm.attempt`/`llm.target`/`llm.circuit`。
- [x] `config/resources.yaml`：三个新字段的注释与示例（不放需要凭证的活动路由）。
- [x] **检查点 G（收尾门）**：五条基线 + 四种 feature 组合 + `cargo tree` 依赖边界 + 全树 ≤500 行。

  **落地记录（第 8 步）**
  - spec：`error-handling.md` 增「Retry and fallback legality」（四条合法性事实、`ResponseCommitted`
    的类型级判定、单行门不变量）；`pingora-guidelines.md` 写尝试循环的挂点与在途状态；
    `logging-guidelines.md` 增 `llm.target`/`llm.attempt`/`llm.circuit` 字段与三档书写规则（每尝试一条
    `info`、每跳过一条 `info`、每相位变化一条 `warn`）；`directory-structure.md`、`quality-guidelines.md`
    与 `index.md` 同步模块与门。
  - `config/resources.yaml`：`retry_budget`（默认 1、上限 4）、`fallback`（上限 3、成员必须可服务）、
    `circuit_breaker`（默认 5 / 30 000 ms、`0` 关闭）的注释与示例，并写明 passthrough 拒绝与 64 KiB
    重放边界；不放需要凭证的活动路由。
  - 检查点 G（真实输出）：`cargo test --workspace --all-features --locked` **1699 passed / 57 suites**（1 ignored）；
    `cargo +nightly fmt --all -- --check` 通过；`cargo clippy --workspace --all-targets --all-features
    --locked -D warnings` 无 issue；`RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps` 通过；
    `cargo deny check`（advisories/bans/licenses/sources）ok；`cargo check --workspace --no-default-features`、
    `cargo check -p nico-server --no-default-features --features traffic`、`cargo check --workspace
    --features llm`、`cargo check --workspace --all-features` 四种组合通过；
    `cargo test -p nico-server --no-default-features --features traffic --test role_traffic` 3 passed；
    `cargo tree -p nico-core` 与 `-p nico-config` 深度 1 无越界项；全树无文件 >500 行（最大 500）。

## 复审重点（每步之后）

1. **canonical 只解码一次**：任何路径上出现第二次 body 解码即设计违背。
2. **发头即单行门**：任何"头已写还继续尝试"的路径都是缺陷，不是策略。
3. **nico-http 词汇**：出现 retry/breaker/fallback 语义（而非字节与次序）即越界。
4. **成功路径零变化**：未配置 retry/fallback/breaker 的路由，一次成功请求的字节与阶段 1 逐字节一致。
   失败路径有意变化：内置熔断默认（threshold 5）在连续失败后跳过 endpoint，见 design §5；显式
   `failure_threshold: 0` 才恢复到阶段 1 的"永不跳过"。
5. **文件预算**：出现第 501 行按数据边界拆分。
