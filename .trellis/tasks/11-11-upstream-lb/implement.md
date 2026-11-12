# 实施计划：流量层负载均衡

> 顺序按"配置 → 选择 → 运行态 → 失败 → 验证 → 文档"排。每步有检查点；任一检查点红了就停下修，
> 不带着红点进下一步。不提交（提交由用户决定）。

## 第 0 步：复核三个假设

- [ ] `Destination` 的构造点与读取点：`proxy/forward.rs::destination`（唯一构造）、
      `upstream::peer`（读 `host` 解析 + SNI）、`upstream::host_header`（读 `host` + `port`）。
      若还有第四处读 `Destination.host` 当身份用，先列出再改。
- [ ] `ServerConf::max_retries` 是否真的只由 LLM 链决定（traffic 路由的重试是 Pingora 自己的
      `max_retries`，本次选成员不改变尝试次数）——确认后决定是否需要动
      `MAX_ATTEMPTS_PER_ROUTE`（预期：不需要）。
- [ ] `schema_bounds` 的字段表（`Bound` 枚举）与默认值的唯一来源，确认加一个
      `max_endpoints` 只改一处。

**回滚点**：若发现 `Destination.host` 还在别处以"身份"含义被读取，停止实现回到设计——那说明
`authority` 不是一个局部字段，而是需要一次更大的拆分。

## 第 1 步：配置模型（`nico-core`）

- [ ] `RawUpstream` 增可选 `endpoints: Vec<RawEndpoint>`；`RawEndpoint { name, address, port?, weight? }`，
      仍 `deny_unknown_fields`。
- [ ] `Upstream` 变成"逻辑身份 + `Arc<[Endpoint]>`"；单实例写法在解析期展开为一个成员
      （`name: "default"`，`address = host`，`port`、`weight = 1`）。
- [ ] 校验：成员非空、名字唯一、`address` 合法（`validate_host` 或 IP 字面量）、`weight` `1..=1000`、
      成员数 ≤ `Bound::Endpoints`（新字段，默认 64）。
- [ ] `Upstream::endpoints()`、`Endpoint` 的访问器；`Destination::from(&Upstream)` 若不再可能，
      删除它并让调用方显式选择（编译错误就是清单）。
- [ ] 单测：单实例展开形状、重名被拒、权重 0 与 1001 被拒、`max_endpoints` 边界、未知字段被拒。

**检查点 A**：`cargo test -p nico-core --lib` 绿；`cargo check --workspace` 的报错清单与第 0 步
列出的构造点一致。

## 第 2 步：`Destination.authority`（`nico-http`）

- [ ] 加字段与 `with_authority`；`Destination::new` 保持 `authority == host`（既有调用零改动）。
- [ ] `host_header()` 与 `peer()` 的 SNI 改读 `authority`；`resolve()` 继续读 `host`。
- [ ] 单测：默认 authority == host；设置了 authority 时 `host_header` 与 SNI 都是它，而解析目标仍是
      `host`（用 IP 字面量 + 域名 authority 的组合断言）。

**检查点 B**：`cargo test -p nico-http --lib` 绿；`cargo test -p nico-llm --lib` 绿（零改动）。

## 第 3 步：选择器与运行态

- [ ] `upstream/pools.rs`：`UpstreamPools`（每 upstream 一份状态）、成员身份四元组、按旧状态对齐新成员
      的函数（纯函数，输入新旧成员表 + 旧状态，输出新状态 + 被丢弃的成员名）。
- [x] `upstream/picker.rs`：`Picker` trait（`pick` + `inherit`）、`Members`/`View` 访问器、四个自带状态
      的实现，以及 `build` 这一处穷尽的 `Strategy → impl` match（`Strategy` 因此去掉
      `#[non_exhaustive]`，新增变体必须编译失败）。
- [ ] `least_conn` 的 guard：在 `HttpContext` 上挂一个 `Drop` 递减的类型；选中时创建。
- [ ] `forward::destination` 插入选择：`upstream → picker.pick → Destination{ host: address,
      authority: upstream.host() }`；无健康成员 → `HttpError::UpstreamFailed`（502）。
- [ ] `HttpApp` 持有 `Arc<UpstreamPools>`（进程级），发布链路把新成员表对齐进去。
- [x] 单测：四策略的选择序列；平滑加权 `3:1:1` 的比例与顺序；会合哈希增删成员时的迁移量；
      继承的四种情形（保留/改地址/删/增）；策略变更不继承状态。

**检查点 C**：`cargo test -p nico-http --lib` 绿；单实例文档的既有 e2e（`http_routing`、`http_body`、
`llm_convert`）零改动通过。

## 第 4 步：失败计入、剔除与观测

- [ ] 失败计入点：`fail_to_connect`（连接层）、`upstream_peer` 的 `Err`（解析）、以及响应阶段的 5xx
      （`upstream_response_filter` 或 `error_while_proxy` —— 二者选一，只在一处计）。
- [ ] 状态机：连续失败达 `max_fails` 剔除、冷却后半开只放一个试探、成功清零、4xx 不计入。
- [ ] 全部剔除 → `502 upstream_unreachable`（不落到连接函数）。
- [ ] 指标 `gateway_upstream_selected_total` / `gateway_upstream_ejected_total`，标签只用配置里的
      名字；日志加 `upstream.endpoint`（仅多成员时）。
- [ ] 单测：状态机四态、4xx 不计入、成功清零、全部剔除的对外 code。

**检查点 D**：`cargo test -p nico-http --lib` 绿；`cargo test -p nico-server --test ops_surface` 绿
（新指标不破坏既有 `/metrics` 断言）。

## 第 5 步：e2e

- [ ] `crates/nico-server/tests/upstream_lb.rs`：一个 upstream 两个实例（两个真实监听器），
      `max_fails = 1`、`fail_timeout_ms` 短：杀掉 A → 后续请求全部落在 B（断言 B 收到的次数），
      A 恢复后要等冷却结束才重新被选中。
- [ ] `crates/nico-server/tests/upstream_lb_tls.rs`（或复用 TLS fixture）：两个 https 实例、证书签逻辑
      host、`Host` 与 SNI 都是逻辑 host，`ip_hash` 下同一客户端稳定落在同一实例。
- [ ] 回归：单实例文档的所有既有 e2e 零改动。

**检查点 E**：新测试连跑 10 次稳定；既有 e2e 全绿。

## 第 6 步：文档与 spec

- [ ] `README.md`：`upstreams[]` 的 `endpoints` 写法、逻辑 `host` 与连接 `address` 的区别、
      四种策略、`max_fails` / `fail_timeout_ms` 语义、"全部被剔 → 502"。
- [ ] `.trellis/spec/backend/pingora-guidelines.md`：选择点、`Destination.authority` 与 SNI 的来源、
      全剔 502 行；`logging-guidelines.md`：两个新指标名与 `upstream.endpoint` 字段；
      `directory-structure.md`：`upstream/{picker,pools}` 的归属。
- [ ] 若加了 `max_endpoints`，`README` 的 `schema_bounds` 段同步。

**检查点 F**：`cargo test -p nico-server --test structure`（若有）与文档断言绿。

## 第 7 步：全量门禁与自评

- [ ] 五条基线；角色矩阵两个组合；`cargo tree -d` 无新增重复栈；全树 ≤500 行
- [ ] 对照 `prd.md` 的验收清单逐条勾销，把证据（测试名 / 命令输出）写进本文件的「落地记录」

**回滚点**：任一步发现选择状态必须进快照才能工作（违反 I5），停止实现回到设计。
