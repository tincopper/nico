# WASM 插件里程碑(研究 + PRD)

## Goal

第三方插件以 WASM artifact 交付并加载,无需重编译或重启网关——实现设计文档 §11 确认的需求,
同时不破坏"插件系统只有一套"的约束:内置插件(Rust)与 WASM 插件都是配置里同一个具名集合的
条目,核心与数据面不为 WASM 加分支。

## Background

现状与约束:

- 插件契约(`nico-http::plugin`):`Plugin` 五阶段(`on_request` / `on_request_body` /
  `on_response` / `on_response_body` / `on_logging`),`PluginContext` 值类型契约(不出现任何
  框架类型),`PluginFactory` 按 `kind` 注册、编译期构建实例,`PluginPhase` 三档排序,
  `Outcome::{Continue, Respond}` 与 `PluginError::{Refused, Execute, PhaseViolation}`。
- registry 注释已预留:"Run time (a later milestone). A WASM host registers one factory that
  loads modules by path; the chain, the phases and Outcome stay unchanged."
- 设计文档 §11 约束:独立 `nico-wasm` crate;版本/签名/权限/资源限制/沙箱预算由该层负责,
  不反向污染 `nico-core` / `nico-http`;核心只校验形状与引用;不做 Rust dylib。

运行时选型结论(本轮研究):

- **wasmi**(解释器):纯解释执行,无 JIT——编译期零成本、确定性、天然免疫 JIT-bomb 类攻击,
  内建 fuel 计量(指令级),双重安全审计(SRLabs 2023 / Runtime Verification 2024),Stellar
  Soroban 生产背书;API 镜像 wasmtime。同步调用模型与插件的 async trait 天然适配(trait 方法
  在锁外驱动同步解释)。
- wasmtime(JIT):热路径吞吐高一个量级,但 JIT 编译期在请求路径上不可接受(需预编译缓存)、
  引入 JIT 内存与攻击面、依赖重;其优势(超低延迟热点)对插件场景不成立——插件逻辑是短小的
  策略判断,不是计算密集内核。
- Extism(宿主框架):自带 PDK/超时/HTTP 宿主函数,但它有自己的 ABI 与变量存储,且底层绑定
  wasmtime——引入它等于把运行时选型与 ABI 都外包,与"宿主 ABI 随本任务 PRD 定义"冲突。

**业界参照(aiway,2026-11-23 从其仓库与文档核实)**:Rust 编写的 AI 网关,选了 **wasmtime 46**
+ 实例池化(PoolingAllocationConfig:1024 实例/内存,单实例 2 MiB 上限)+ **拉取式宿主函数**
(`aiway::*` 约 20 个 func_wrap,插件经 `aiway_call(hook_id)` 跨 FFI 后按需从 HttpContext 读
事实)+ 五钩子全开放 + reqwest 出网 + SHA-2 哈希。文档明确承认 FFI 每次调用的上下文切换损耗,
建议限制热路径插件数。

**结论:wasmi + 自定义极简宿主 ABI。** fuel 计量承担沙箱预算(指令数即 CPU 预算),内存上限
由 wasmi `Config` 承担,超限 → `PluginError::Execute`。aiway 的实践验证了 WASM 插件网关的
可行形状,同时反证了三点:它的 wasmtime+池化是为热路径吞吐付出的复杂度(池化实例/FFI 指针
注入/UnsafeCell),nico 的插件是短策略逻辑,wasmi 的确定性与 fuel 计量更匹配;它的拉取式宿主
函数暴露了大量 HttpContext 面,本任务的推送式单字节块 ABI 更小;它五钩子全开,nico M1 只开
三个非流式阶段。

## Requirements

### R1|schema:插件实例的 `wasm` kind

- `plugins[]` 增加 `kind: wasm` 实例:配置携带 `module`(路径,发布期读取并编译)、`fuel_per_call`
  (单次调用的指令预算)、`memory_pages`(上限,默认 16 ≈ 1 MiB)、可选 `config`(透传给插件的
  JSON,与内置插件同一形状)。
- 拒绝路径与内置插件完全一致:未知 kind 是发布失败;`wasm` 模块读不到/编译失败 → 发布失败,
  LKG 保持。
- 发布期编译一次模块,实例按代缓存(与 rate-limit 的桶表同规则:编译产物是运行态,跨 reload
  继承)。

### R2|宿主 ABI(极简字节进出)

- 插件导出按阶段命名:`on_request` / `on_response` / `on_logging`;请求体阶段(M1)不开放
  (流式 body 的跨边界字节泵是独立能力)。
- 调用形状:宿主写入输入字节(该阶段可见的事实 JSON:method/path/headers/identity/……),调用
  导出函数,读回输出字节(动作 JSON:`continue` | `respond{status,headers,body}`)。
- 无 WASI、无文件系统、无时钟、无网络:插件能看见的只是宿主喂给它的字节。宿主函数只有内存
  分配对(`nico_alloc`/`nico_free` 语义)与按需扩展的事实读取。
- 阶段映射:配置声明 `phase`(authenticate/limit/other)→ `PluginFactory::phase()`;五阶段中
  WASM 只参与有导出的阶段,未导出 = 该阶段 Continue(与内置 `PluginPhases` 同规则)。

### R3|失败与资源边界

- fuel 耗尽 / 内存超限 / trap → `PluginError::Execute`(500 路径,与内置插件故障同桶),
  详情进日志,绝不进响应。
- 每次 callsite 调用独立 fuel;实例不跨请求缓存可变状态(每代重建,与内置插件的无状态实例
  同规则)。
- `deny by default`:wasmi Config 关闭除 MVP 外的所有提案(SIMD/多内存/线程等)。

### R4|范围外(本里程碑不做)

- 请求体/响应体阶段的 WASM(流式字节泵);签名与供应链校验(路径+哈希 pin 已够 M1,签名另立);
  WASI;多模块组合;热路径 JIT(wasmtime)升级路径;插件间通信。

## Acceptance Criteria

- [ ] AC1:schema——`kind: wasm` 合法携带 module/fuel/memory/config;模块缺失/编译失败发布拒绝,
      LKG 保持;provider 无关(插件域不变)。
- [ ] AC2:E2E——一个 Rust→WASM 编译的测试插件:authenticate 阶段拒绝无凭据请求(Refused 形状
      与内置同)、respond 动作产出合成应答、正常请求 Continue。
- [ ] AC3:fuel 耗尽的插件 → 500(Execute 路径),进程不死,后续请求继续被拒但网关健康;
      trap 同路径。
- [ ] AC4:两个 WASM 实例 + 一个内置插件在同一链上按 phase/文档序正确执行。
- [ ] AC5:依赖边界——`nico-wasm` 新 crate,只有 `nico-plugins` 依赖它;`nico-core`/`nico-http`
      零改动(设计 §11 验收);feature 门控(默认关,与 etcd 同规则)。
- [ ] AC6:门禁全绿;`cargo tree -d` 无新增重复栈。

## Notes

- 实现顺序:先 `nico-wasm`(host 环绕 wasmi + ABI),再 `nico-plugins` 的 factory 注册,
  再 E2E;测试插件用 Rust 编译为 wasmi 可载的 wasm(仓库内 fixture,构建脚本或预编译入库)。
- 治理层(虚拟 key/租户)不在本任务:它以插件实现的方向不变,但那是 WASM 落地后的第二个消费者。
