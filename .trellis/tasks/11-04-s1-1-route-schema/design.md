# 路由 schema 扩展与四段校验 — 技术设计

> 目标：把 `gateway-core::Document` 从 `apiVersion + limits` 扩展成阶段 1 唯一的配置模型
> （`routes` / `upstreams` / `providers`），并把校验固定为「解析 → 引用校验 → 语义校验 → 构造」。
> 本文只描述最终结构；需求与验收见 `prd.md`。

## 1. 设计约束

- `gateway-config` 不做改动；`gateway-server` 只新增 `schema_bounds` → `ParseLimits` 的接线，
  reload 管线与 CLI 语义不变。`gateway-core` 的公开 API 只增不改（通过
  `lib.rs` 重导出保持既有路径）。
- 文件预算 300–500 行，硬上限 500（见 `spec/backend/quality-guidelines.md`）。当前 `config.rs`
  670 行，新增三个实体后必然超限，因此本任务**先拆分再扩展**，两次改动分开提交。
- 配置是不可信输入：所有边界在解析期强制，绝不依赖调用方。
- 本任务不产出路由匹配表（`CompiledRoute` 属 `1.2 http-data-plane`），只产出经过校验的文档模型。

## 2. 模块结构

拆分后 `gateway-core` 的结构（新文件均远低于 300 行）：

```text
crates/gateway-core/src/
├── lib.rs               # 重导出（对外 API 路径不变）
├── version.rs           # Generation / GenerationOverflow / Revision / RevisionOrdering / ConfigDigest
├── error.rs             # ErrorCategory（已有）
├── status.rs            # ConfigStatus（已有）
├── snapshot.rs          # RuntimeSnapshot / SnapshotHandle（已有）
├── config.rs            # 模块入口：mod 声明 + 重导出 + 模块文档
└── config/
    ├── document.rs      # Document（集合与 accessor）；document/tests.rs 放解析级测试
    ├── limits.rs        # 文档内 `limits:` 段（运行时请求上限）
    ├── bounds.rs        # ParseLimits / SchemaBounds / BoundsError（解析期边界）
    ├── provider.rs      # Provider / ProviderKind
    ├── upstream.rs      # Upstream / Scheme
    ├── route.rs         # Route / RouteTarget
    ├── raw.rs           # serde 反序列化层：Raw* 形状，deny_unknown_fields
    ├── parse.rs         # 字节 → 边界 → alias → Value → 节点/深度 → RawDocument
    ├── validate.rs      # 校验编排：包络 → 目标 → 引用 → 语义 → 数量 → 构造
    ├── reference.rs     # 引用校验：id 唯一性、目标存在性
    ├── semantic.rs      # 语义校验：命名、path、scheme、端口、数值区间、组合唯一性
    └── error.rs         # DocumentError 及 ErrorCategory 映射
```

拆分动机（每条对应一个业务边界或数据结构）：

- `version.rs` 只拥有「版本标识」：generation/revision/digest。它们与文档模型无关，被快照与管线共用。
- `config/document.rs` 只拥有**已校验**的领域模型与 accessor。
- `config/raw.rs` 只拥有 serde 形状，含 `deny_unknown_fields`；它永远不逃出 `config` 模块。
- `parse.rs` 拥有「字节 → 类型」的边界与资源上限；`validate.rs` 拥有「类型 → 合法领域值」的规则。
  引用校验与语义校验是两类独立决策（谁是坏的：引用了不存在的东西 vs 字段本身非法），因此分文件。

`lib.rs` 是唯一的对外路径：`config::{...}` 继续重导出文档模型，`version::{...}` 重导出
`Generation` / `Revision` / `RevisionOrdering` / `ConfigDigest`，并新增 `Bound` 与
`Provider`、`ProviderKind`、`Route`、`RouteTarget`、`Upstream`。`config` 不重导出 `version` 的项：
配置文档模型与快照版本标识是两个边界，`gateway_core::Revision` 这类路径由 `lib.rs` 从拥有者
`version` 导出（`gateway-config` 与 `snapshot.rs` 因此从 `gateway_core::version` 取用）。

## 3. 配置模型

```rust
// config/document.rs（示意）
pub struct Document {
    api_version: String,
    limits: Limits,
    providers: Vec<Provider>,
    upstreams: Vec<Upstream>,
    routes: Vec<Route>,
}

pub struct Limits { max_request_body_bytes: u32, request_timeout_ms: u32 }

pub struct Provider {
    name: String,
    kind: ProviderKind,          // 本阶段只有 OpenAi
    base_url: String,
    credential_ref: String,      // env:VAR | file:/path
    model_allowlist: Vec<String>,// 空 = 不限制（阶段 1 不校验模型存在性）
    timeout_ms: Option<u32>,
}

#[non_exhaustive]
pub enum ProviderKind { OpenAi }

pub struct Upstream { name: String, scheme: Scheme, host: String, port: u16 }

#[non_exhaustive]
pub enum Scheme { Http, Https }

pub struct Route {
    id: String,
    host: Option<String>,
    path_prefix: String,
    target: RouteTarget,   // 构造时保证恰好一个目标
    timeout_ms: Option<u32>,
    max_body_bytes: Option<u32>,
}

#[non_exhaustive]
pub enum RouteTarget { Upstream(String), Provider(String) }
```

Serde 规则：

- 全部 `#[serde(deny_unknown_fields)]`；文档级结构不使用 `serde(default)`。
- `ProviderKind` / `Scheme` 用小写 `#[serde(rename_all = "lowercase")]`，未知值由 serde 报错
  （错误归类为 `Client`）。
- `credential_ref` 只是引用字符串，本任务**不解析**、不读取环境或文件；解析属 `1.4 gateway-ai-passthrough`。
- `model_allowlist` 在文档中可省略，缺省等价于空切片，语义为「不限制」；阶段 1 不校验模型名是否被
  provider 支持。省略用 `Option<Vec<String>>` 表达，不用 `serde(default)`，保持「键不存在」与「键存在但为空」
  在反序列化层可区分。
- 路由目标用两个可选字段表达（`upstream` / `provider`），在引用校验阶段转成 `RouteTarget`，
  这样「缺目标」与「两个目标」可以分别给出可读错误，而不是依赖 `untagged` 的模糊报错。

## 4. 解析与边界

`Document::parse(bytes, limits)` 的固定顺序：

1. **字节上限**：`bytes.len() > limits.max_bytes` → `TooLarge`。
2. **UTF-8**：非法 → `NotUtf8`。
3. **alias/anchor 拒绝**：配置文档禁止 YAML 锚点与别名。逐字节扫描，仅在「token 起始位置」
   （行首或前置分隔符为空白、`[`、`{`、`,`、`-`）出现 `&` 或 `*` 后紧跟标识符字符时判定为违规 →
   `AliasForbidden`。理由：锚点展开可以在 `max_bytes` 之内产生远超预算的节点，节点上限是事后检查，
   挡不住展开本身。
4. **YAML → Value**：`serde_norway::from_slice::<serde_norway::Value>`；语法错误 → `Yaml`。
5. **节点数与深度**：在 `Value` 上做一次**迭代式**遍历（显式栈，不用递归，防止对手构造深文档把
   自己的调用栈打爆），统计节点数（标量、序列元素、映射键值对）与最大深度；超限 →
   `TooManyNodes` / `TooDeep`。`serde_norway` 自身有更低的递归上限，因此极深的文档可能在解析阶段就以
   `Yaml` 失败而不是 `TooDeep`；`max_depth` 只在可达范围内生效（实测约 33 起可触发，测试按此断言）。
6. **形状**：`RawDocument::deserialize(value)`；未知字段/类型错误 → `Yaml`。
7. **四段校验**（§5）：把 `RawDocument` 转成 `Document`。

上限分两层，各管一件事：

```rust
// config/bounds.rs
pub struct ParseLimits { max_bytes: usize, schema: SchemaBounds }

pub enum Bound { Routes, Upstreams, Providers, Nodes, Depth }

pub struct SchemaBounds {
    max_routes: NonZeroUsize,
    max_upstreams: NonZeroUsize,
    max_providers: NonZeroUsize,
    max_nodes: NonZeroUsize,
    max_depth: NonZeroUsize,
}

impl SchemaBounds {
    pub const fn get(self, bound: Bound) -> usize;
    pub fn with(self, bound: Bound, value: usize) -> Result<Self, BoundsError>;
}
```

- `max_bytes` 属于**传输/读取**边界（来源给多少字节），因此留在 `ParseLimits` 顶层，由
  `config_source.max_bytes` 提供。
- 其余五项属于**文档形状**边界（schema 有多复杂），放进 `SchemaBounds`，与 bootstrap 段名
  `schema_bounds` 一一对应。
- 用 `bounds` 而不是 `limits`：`limits` 在本项目已经承载两个更常见的含义——文档里的 `limits:` 段是
  **运行时请求**上限，限流策略也围绕 limit 展开；`bounds` 专指「解析期允许的规模上限」，不会与它们混读。
- 不用 `schema_config`：那会读成「schema 的配置」，与已有的 `Document`/`ConfigSource`/`config` 模块名
  挤在同一语义空间；`bounds` 明确表达「这里是数值边界，不是可配置对象」。
- `ParseLimits` 保留原名：它是阶段 0 就存在的公开类型，表示「解析期资源上限（字节 + 形状边界）」，
  改名只会波及 `gateway-server` 与测试而无收益。

| 字段 | 默认值 | 说明 |
|---|---|---|
| `max_routes` | 1024 | 路由条数上限 |
| `max_upstreams` | 256 | upstream 数量上限 |
| `max_providers` | 64 | provider 数量上限 |
| `max_nodes` | 100_000 | YAML 节点数上限（1 MiB 文档的合理上限） |
| `max_depth` | 32 | 嵌套深度上限（典型配置 ≤ 6） |

默认值只定义一次：`impl Default for SchemaBounds`。`ParseLimits::new(max_bytes)` 使用
`SchemaBounds::default()`；bootstrap 的缺省值也从它派生，禁止第二处硬编码。

不变量由类型承担：字段是 `NonZeroUsize`，`0` 不可表示；唯一写入路径 `with(bound, value)` 对 `0`
返回 `BoundsError`（携带字段名与拒绝值），所以「解析器接受了一个永远无法满足的上限」不成立。
默认值只有 `SchemaBounds::default()` 一处，构造路径是「默认值 + 逐项覆盖」。

一个 `Bound` 变体承载字段名（`Bound::name()` 就是配置键，如 `max_routes`），因此读、写、报错三处
不会各自维护一张字段表；新增一个边界是加一个变体加一个 `match` 分支，而不是复制两段访问器。
`Bound` 不加 `#[non_exhaustive]`：新增边界必须让每个消费方决定自己的取值范围，这个编译期义务不
应退化成通配符分支。

`ParseLimits::new(max_bytes)` / `with_schema(schema)` 是管线唯一的上限入口。

### 4.1 bootstrap 接线

运维可见的上限放在本地 bootstrap 的新段落（不是网关资源文档，避免配置中心能放宽自己的边界）：

```yaml
# config/bootstrap.yaml —— 整段可省略，也可只写部分字段
schema_bounds:
  max_routes: 1024        # 1..=1_000_000
  max_upstreams: 256      # 1..=1_000_000
  max_providers: 64       # 1..=1_000_000
  max_nodes: 100000       # 1..=10_000_000
  max_depth: 32           # 1..=128
```

- `RawBootstrap.schema_bounds: Option<RawSchemaBounds>`，字段也全部是 `Option<usize>`：整段省略是
  `None`，某个字段省略是 `None`，两种情况都表示「不覆盖」，从 `SchemaBounds::default()` 起步。
  默认值因此只在 `SchemaBounds::default()` 存在一份，bootstrap 里没有任何数字副本。
  **未知字段仍然被拒绝**（`deny_unknown_fields`），拼错的键不会被默认吸收。
- `RawSchemaBounds::resolve` 只做一件事：遍历 `[(Bound, Option<usize>); 5]`，对写了的项先查
  `1..=maximum(bound)`，再交给 `SchemaBounds::with`。字段名来自 `Bound::name()`，消息里的上限来自
  `maximum(bound)`，检查与文案不可能不一致。
- `maximum(bound)` 是 bootstrap 的策略上限，不是 `SchemaBounds` 的性质：
  `Routes`/`Upstreams`/`Providers` ≤ 1_000_000，`Nodes` ≤ 10_000_000，`Depth` ≤ 128。它用于拦截手误
  （例如 `max_depth: 1000000000`），不是业务约束；越界统一报 `BootstrapError::InvalidField`。
- `BootstrapConfig::parse_limits(&self) -> ParseLimits`：`max_bytes` 取 `config_source.max_bytes`，
  `schema` 由上面合并结果给出。`gateway-server` 的构建处只调用它。
- 向后兼容：未写 `schema_bounds` 的既有 `bootstrap.yaml` 继续可用（取默认值），不需要迁移。

## 5. 四段校验

### 5.1 引用校验（`reference.rs`）

| 规则 | 错误 |
|---|---|
| `route.id` 全局唯一 | `DuplicateId { kind: "route", id }` |
| `upstream.name` / `provider.name` 内部唯一 | `DuplicateId { kind: "upstream"/"provider", id }` |
| 路由必须且只能有一个目标 | `RouteTargetMissing { id }` / `RouteTargetAmbiguous { id }` |
| `route.upstream` 必须存在于 `upstreams` | `ReferenceUnknown { kind: "upstream", name }` |
| `route.provider` 必须存在于 `providers` | `ReferenceUnknown { kind: "provider", name }` |

`ReferenceUnknown` 的 `kind` 使用 `&'static str`（`"upstream"` / `"provider"`），避免为一个两值集合
引入新枚举；错误消息里直接出现 `kind` 与 `name`。

### 5.2 语义校验（`semantic.rs`）

| 规则 | 错误 |
|---|---|
| 名称（upstream / provider / route id）匹配 `[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?` | `InvalidField { field, reason: "must be lowercase kebab-case" }` |
| `routes[].host` 非空、无 scheme/端口/空白、长度 ≤ 253 | `InvalidField { field: "routes[].host" }` |
| `upstreams[].host` 非空、无 scheme/端口/空白、长度 ≤ 253 | `InvalidField { field: "upstreams[].host" }` |
| `path_prefix` 以 `/` 开头、无 `..`、无空白、长度 ≤ 256 | `InvalidField { field: "routes[].path_prefix" }` |
| `(host, path_prefix)` 组合唯一（`None` host 视作 `"*"`） | `DuplicateId { kind: "route-match", id }`（id 为组合的可读形式） |
| `timeout_ms` 在 `1..=600_000` | `LimitOutOfRange { field, value }`（既有变体） |
| `route.max_body_bytes` ∈ `1..=limits.max_request_body_bytes`（0 表示「不允许任何 body」，是误配） | `LimitOutOfRange` |
| `provider.base_url` 以 `http://` 或 `https://` 开头、无空白 | `InvalidField { field: "providers[].base_url" }` |
| `provider.credential_ref` 以 `env:` 或 `file:` 开头 | `InvalidField { field: "providers[].credential_ref" }` |
| `upstream.port` 非 0（`u16` 已保证范围） | `InvalidField { field: "upstreams[].port" }` |
| 数量上限 | `TooMany { kind, count, limit }` |

`ProviderKind` 与 `Scheme` 不在这一层校验：它们是封闭枚举，未知拼写由 serde 在解析阶段拒绝，
报 `Yaml`（类别 `Client`）。

`reason` 里出现的数字一律由规则常量格式化（`format!("must be at most {MAX_HOST_BYTES} bytes")`），
不写死字面量：常量改动后文案随之改动，不会被测试之外的人发现不一致。

`path_prefix` 的 `/` 结尾语义（是否等价于目录前缀）由 `1.2 http-data-plane` 定义；本任务只校验
语法合法性，不定义匹配后缀规则，避免把匹配策略写进 schema。

### 5.3 构造

`Document` 只能经 `parse` 得到；字段私有 + accessor。任何阶段失败都不产生 `Document`。
`Document::routes()` / `upstreams()` / `providers()` 返回切片，调用方无法在文档内插入未校验条目。

## 6. 错误与分类

`DocumentError` 新增变体（`thiserror`，`Debug`，`PartialEq` 不强制——含 `serde_norway::Error` 的变体
保持不可比较，其余用 `matches!` 断言）：

| 变体 | 类别 |
|---|---|
| `ReferenceUnknown { kind: &'static str, name: String }` | `Client` |
| `DuplicateId { kind: &'static str, id: String }` | `Client` |
| `RouteTargetMissing { id: String }` | `Client` |
| `RouteTargetAmbiguous { id: String }` | `Client` |
| `InvalidField { field: &'static str, reason: Cow<'static, str> }` | `Client` |
| `AliasForbidden` | `Client` |
| `TooMany { kind: &'static str, count: usize, limit: usize }` | `Limit` |
| `TooManyNodes { count: usize, limit: usize }` | `Limit` |
| `TooDeep { depth: usize, limit: usize }` | `Limit` |

既有变体（`TooLarge` / `NotUtf8` / `WrongApiVersion` / `Yaml` / `LimitOutOfRange`）保持不变。
`reason` 是 `Cow<'static, str>`：固定句子借用，引用上限的句子自己持有，于是常量与文案不会脱钩，
而无数字的规则仍然零分配。所有变体继续满足 `Send + Sync + 'static`（编译期断言保留）。

## 7. 兼容与迁移

- **公开 API**：`gateway_core::{Generation, Revision, RevisionOrdering, ConfigDigest, Document,
  DocumentError, Limits, ParseLimits, API_VERSION}` 全部保持可用，新增 `Bound`。版本标识的对外路径
  由 `lib.rs` 从 `version`（拥有者）导出，`snapshot.rs` 与 `gateway-config` 因此把 import 指向
  `gateway_core::version`；这是路径收敛，不是行为变化。`gateway-server` 只改两处：`bootstrap.rs`
  增加 `schema_bounds` 解析与 `parse_limits()`，以及把构造 `ParseLimits` 的调用替换为它。
  reload 管线、缓存与 CLI 语义不变。
- **示例配置**：`config/resources.yaml` 增加一条普通 API 路由（指向 `127.0.0.1:8080`）与一条 AI 路由
  （引用 `openai-primary`），用于展示 schema；不要求真实上游可连通（无数据面）。
  `config/bootstrap.yaml` 可选地展示 `schema_bounds` 段（注释说明可省略、默认为 `SchemaBounds::default()`）。
- **CI 基线**：五个门禁命令与现有 127 个测试必须保持全绿。

## 8. 测试设计

| 层 | 覆盖 |
|---|---|
| 拆分回归 | 拆分提交后 `cargo test --workspace` 全绿，公开 API 路径不变（`use gateway_core::{...}` 的既有测试即证明） |
| 解析 | 合法示例；未知字段（文档级/路由级/上游级）；错误 `apiVersion`；超 `max_bytes`；非 UTF-8；YAML 语法错误；空文档 |
| alias | `&anchor` / `*alias` 被拒；字符串内含 `*`/`&`（如 `"a*b"`）不被误判 |
| 边界 | `max_routes` / `max_upstreams` / `max_providers` / `max_nodes` / `max_depth` 各一例，断言 `Limit` 类 |
| 引用 | 未知 upstream/provider、缺目标、双目标、重复 route id、重复 upstream 名、重复 provider 名 |
| 语义 | 名称非法、host 含端口/空白、`path_prefix` 不以 `/` 开头/含 `..`、重复 `(host, path_prefix)`、timeout 越界、`max_body_bytes` 超文档上限、非法 scheme、非法 `base_url`、非法 `credential_ref`、port 0 |
| 属性 | proptest：任意字节不 panic（沿用阶段 0 用例）；任意合法文档经 `parse` 后 `routes().len()` 不超过 `max_routes` |
| 文档 | `Document::parse` 的 doc test 更新为带 routes 的示例（成功 + 失败各一） |
| bootstrap | 整段省略 → 全默认；部分字段 → 其余取默认；未知字段被拒；`0` 与「超过策略上限」对每个 `Bound` 各一例；`gateway-server` 端到端用例：`max_routes: 1` + 2 条路由 → `degraded`、generation 不变 |
| 默认值单一来源 | 默认值只在 `SchemaBounds::default()` 定义，由它的单元测试固定；集成测试证明省略段落即取该默认 |

测试文件拆分遵循 500 行上限：`config/parse.rs`、`config/reference.rs`、`config/semantic.rs` 的
`#[cfg(test)]` 模块各自就地放置，若单个测试模块超过 200 行则转 `tests/` 下的场景文件。

`crates/gateway-server/tests/` 的三个场景文件（`pipeline.rs` / `schema_bounds.rs` / `local_file.rs`）
共享 `tests/support/mod.rs`：一份文档夹具、一个手驱 `ChannelSource`、一个暂停时钟 `Harness` 与
bootstrap 夹具构造器。三个集成测试各自是独立 crate，只编译自己用到的部分，因此该模块显式声明
`#![expect(dead_code, ...)]`；新增/删除夹具时这条豁免随之被校验。

## 9. 风险与处置

| 风险 | 处置 |
|---|---|
| 拆分导致下游 import 断裂 | 只用重导出搬迁，不重命名公开项；每步跑全 workspace 测试 |
| `serde_norway::Value` 二次解析带来额外内存 | 文档上限 1 MiB，Value 峰值在数 MB 量级；节点上限兜底。若实测超预算，改为先 `Deserializer` + 计数 visitor（不引入额外依赖） |
| alias 词法检查误判 | 规则限定「token 起始位置」，含针对性测试（`"a*b"`、`"&&"`、注释中的 `*`）；若仍误判，降级为文档说明而不阻断 |
| `path_prefix` 语义提前固化 | 本任务只校验语法；匹配与优先级归 `1.2`，其 PRD 已要求最长前缀 + host 精确优先 |
| 错误变体过多 | 每个变体对应一个调用方决策（拒绝原因）；同类错误用 `field`/`kind` 区分，不按字段拆变体 |
| 默认值与 bootstrap 各自演化导致漂移 | 默认值只在 `SchemaBounds::default()` 定义；bootstrap 用 `Option` 表达「没写」，不再复制一份默认表 |
| `bootstrap.rs` 逼近 500 行 | 当前 361 行（校验表收敛后比计划更短）；上限表 + `resolve` 合并成一个循环，不引入额外抽象；若接近 500 则先拆 `bootstrap/{raw,validate}.rs` |

## 10. 交付边界

- 只改 `gateway-core` 与 `config/resources.yaml` 示例。
- 不新增 crate、不引入新依赖（`serde_norway` 已在用）。
- 不实现路由匹配、不做 provider 凭证解析、不接 metrics。
