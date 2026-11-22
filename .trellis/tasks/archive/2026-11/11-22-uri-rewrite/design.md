# 设计:traffic upstream 路由的 URI 前缀改写

## 1. 不变量与失败模式

1. **匹配用原路径,转发用新路径。** 改写发生在路由匹配之后、转发之前;路由表、优先级、插件链
   读到的都是客户端原路径——改写对它们不可见。
2. **逐字节转发。** query 与百分比编码不解码、不重编码:新路径 = 配置前缀 + 原路径的
   `path_prefix` 长度之后的字节,query 原样跟随。改写是字节操作,不是 URI 解析重组。
3. **幂等由构造保证。** 每轮从 `ctx.path()`(客户端原始路径)重算,而不是从上一轮改写后的
   请求头——failover 的重试轮不会叠加。
4. **provider 路径不可侵犯。** 转换链的路径由适配器编译,passthrough 的路径是调用方的;
   两种 provider 路由都拒绝 `rewrite_prefix`。

失败模式:

| 失败 | 行为 |
|---|---|
| `rewrite_prefix` 不以 `/` 开头且非空 | 发布失败,点名字段 |
| 含 `?` / `#` / 空白 / 控制字符 | 发布失败——转发期的重组因此不会失败 |
| 写在 provider 路由上 | 发布失败:路径由适配器所有 |
| 客户端路径不含 `path_prefix` 前缀 | 不可能:匹配已保证;若发生按原样转发(防御性,不特判) |

## 2. 契约

```yaml
routes:
  - id: api
    path_prefix: /api/
    upstream: internal
    rewrite_prefix: /internal/   # 可选;剥离整个前缀写 /
```

字段是 flat 的一个值,不是块:最小语义(前缀替换)一个字符串说完;正则/条件改写是另一些
能力,出现时再立任务,不为外观预留嵌套。

## 3. 结构

| 件 | 归属 | 内容 |
|---|---|---|
| `RawRoute.rewrite_prefix` / `Route.rewrite_prefix` | `nico-core` schema | `Option<String>`;语义校验(字符集、斜杠)在 semantic |
| `check_target_fields` | `nico-core` reference | provider 路由拒绝(加入 declared 列表) |
| `HttpContext::rewrite_prefix()` | `nico-http` context | bind_route 时随其他路由事实缓存 |
| `upstream_request_filter` | `proxy/hooks/request.rs` | 每轮从 `ctx.path()` 重算并写回 `request.uri`;`?` 后的 query 原样保留 |

## 4. 测试策略

| 层 | 覆盖 | 位置 |
|---|---|---|
| schema | 非法值矩阵、provider 拒绝、池化路由携带 | `nico-core` 文档校验测试 |
| 转发 | 前缀替换、query 保留、深路径、空串剥离、无改写原样 | `nico-http` forward 单测 + E2E |
| 幂等 | failover 重试轮改写一次 | E2E(failover 路由 + rewrite) |
| 不回归 | 无改写字段的路由全部既有测试 | 既有套件 |

## 5. 被否决的替代方案

| 方案 | 否决理由 |
|---|---|
| 嵌套 `rewrite: {}` 块 | 最小语义一个字符串说完;嵌套是为正则/条件预留的外观 |
| 正则改写 | 审查建议"先选最小需要的";正则的性能与歧义(贪婪/非贪婪)需要自己的任务 |
| provider 路由也支持 | 路径由适配器所有是 llm 域的硬边界;passthrough 的改写语义(改什么?)没有回答 |
| 转发期从改写后的 uri 再改写 | 非幂等:重试轮叠加;从客户端原路径重算是构造性幂等 |
