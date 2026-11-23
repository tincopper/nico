# 设计:WASM 插件宿主(nico-wasm)

## 1. 不变量与失败模式

1. **插件系统只有一套。** WASM 实例是文档里同一个具名集合的条目;registry 多一个 factory,
   chain/phases/Outcome/错误映射零改动。`nico-core` 与 `nico-http` 不认识 wasm。
2. **插件看得见的只有宿主喂的字节。** 无 WASI、无时钟、无网络、无文件——沙箱边界是 ABI 的
   形状,不是运行时的开关。
3. **每次调用有界。** fuel(指令数)+ 内存页上限,两者任一超限 → `Execute`;一次失控插件消耗
   的资源是发布时声明的常数。
4. **编译是发布期事实。** 模块在发布管线里读取并编译(失败 = LKG 保持),编译产物跨代继承;
   请求路径只做实例化 + 调用,零编译成本。
5. **实例无状态。** 每代按模块重建实例集,插件不跨请求携带可变状态——与内置插件同规则。

失败模式:

| 失败 | 行为 |
|---|---|
| module 路径读不到 / wasmi 编译失败 | 发布失败(`plugins[].module`),LKG 保持 |
| fuel 耗尽 / 内存超限 / trap | `PluginError::Execute`(500),日志带 instance+阶段,进程不死 |
| 导出缺失(某阶段) | 该阶段 Continue(与内置未声明阶段同规则) |
| 输出字节不是合法动作 JSON | Execute(宿主的解析即校验) |

## 2. 宿主 ABI

输入(宿主→插件,单块内存):该阶段事实的 JSON——

```json
{"method":"GET","path":"/api/x","headers":{...},"identity":{"subject":"..."}|null,"config":{...}}
```

输出(插件→宿主,单块内存):动作 JSON——

```json
{"action":"continue"}
{"action":"respond","status":403,"headers":{...},"body":"..."}
```

内存约定:插件导出 `nico_alloc(size) -> ptr` 与 `nico_free(ptr, size)`;宿主按
`ptr: *mut u8 + len: i32` 的线性内存约定传参。不含版本协商:ABI 由本文件定义,M1 冻结,
扩展走新导出名(如 `on_request_v2`),不做运行时探测。

## 3. 结构

| 件 | 归属 | 内容 |
|---|---|---|
| `nico-wasm` crate(feature `wasm`,默认关) | 依赖:`wasmi` + `serde_json` | 模块编译缓存、`WasmPlugin`(实现 `Plugin` 五阶段,M1 三阶段)、fuel/内存配置、ABI 编解码 |
| `WasmFactory` | `nico-plugins`(feature `wasm`) | `kind() == "wasm"`;发布期读模块 → `nico-wasm` 编译 → 构建 `WasmPlugin`;`builtin_registry` 保持纯内置,`with_registry` 组装时按 feature 合入 |
| schema | `nico-core`(零改动) | `kind` 是字符串;wasm 的 config 形状由 factory 校验——与现有"core 只校验形状与引用"完全一致 |

依赖方向:`nico-plugins → nico-wasm → (wasmi)`;`nico-http` 不变(它只认识 `Plugin` trait)。
feature 关系:`nico-server` 的 `wasm` feature 开 `nico-plugins/wasm`。

## 4. 测试策略

| 层 | 覆盖 | 位置 |
|---|---|---|
| ABI 编解码 | 事实 JSON → 输入;输出 → 动作;非法动作 = Execute | `nico-wasm` 单测 |
| 沙箱 | fuel 耗尽 → Execute;内存超限 → Execute;导出缺失 → Continue | `nico-wasm` 单测(fixture .wat 手写,免编译链) |
| factory | 模块缺失/坏模块 → 发布失败;config 形状校验 | `nico-plugins` 测试 |
| E2E | 混合链:内置 jwt + WASM authenticate(拒绝/respond)+ Continue 路径;fuel 耗尽 500 且进程存活 | `crates/nico-server/tests/http_wasm.rs` |
| 边界 | `cargo tree`:wasm 依赖只在 feature 后出现;core/http 零改动 | `dependency_boundaries.rs` |

fixture:`.wat` 文本经 wasmi 的 wat 支持编译(或 `wat` crate),手写两个小插件(拒绝器、
回显器),不依赖 Rust→wasm 工具链进入 CI。

## 5. 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| wasmtime(JIT) | 编译期进请求路径(或引入预编译缓存机制),JIT 内存与攻击面,依赖重;插件是短策略逻辑,不是计算内核,解释器的确定性与 fuel 计量正好匹配。业界参照 aiway 恰是此方案(wasmtime 46 + 1024 实例池 + 2 MiB/实例 + 拉取式宿主函数),其文档自承 FFI 上下文切换损耗并建议限热路径插件数——池化与指针注入的复杂度是为热路径吞吐付的价,nico 不需要 |
| Extism | 自带 ABI/PDK/变量存储,等于外包本任务要定义的宿主 ABI;底层绑 wasmtime,选型冲突 |
| Rust dylib | 设计 §11 已否决:ABI 不安全、崩溃域与进程相同、无沙箱 |
| 请求体阶段(WASM M1) | 流式 body 的跨边界字节泵是独立能力,§11 未要求 M1 覆盖 |
| 每请求实例化新引擎 | wasmi 引擎与模块编译是发布期事实;请求路径只实例化 |
