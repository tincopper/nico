# 设计:G4 缓冲与 header 控制

## 1. 不变量与失败模式

1. **只承诺 hook 可实现的子集。** `proxy_buffering` 默认 on，将响应 chunks 扣留到 end-of-stream 后整体交给下游；它不承诺 nginx 临时文件语义或上游连接生命周期与慢客户端解耦。请求缓冲继续使用现有 `buffers_body` 语义，不声称与 nginx request buffering 等价。
2. **spool 有界,超限降级不报错。** M1 只做内存 spool；`max_buffer_size` 超限时，spool 保留已累计前缀，触发超限的当前 chunk 与该前缀合并后输出，之后切换为逐 chunk 透传。
3. **改写在自动行为之后。** `X-Forwarded-*` 与自动 `Host` 先应用,路由级 `proxy_set_header` 后应用:路由级显式意图覆盖自动值。凭证头(`authorization`/`cookie`)可被覆盖(显式路由意图),hop-by-hop 仍由框架清除。
4. **LLM 路由不参与。** convert 的整体缓冲与事件流透传是协议语义;passthrough 也不参与(路径与凭证头由适配器所有)。新配置都是 upstream 路由词汇,provider 路由写入即发布失败。
5. **响应 head 已发送后不改变状态码。** body 缓冲在响应头过滤之后发生；超限采用透传降级，不新增运行时错误路径。

| 条件 | 行为 |
|---|---|
| header name 非 token / value 非可见 ASCII | 发布失败,点名字段 |
| 改写 `content-length`/`transfer-encoding` | 发布失败: framing 由框架管理 |
| `max_buffer_size` 为 0 或超发布界 | 发布失败 |
| provider 路由写任一新字段 | 发布失败: upstream 路由专属配置 |
| `proxy_buffering` on | chunks 扣留到 end-of-stream 后整体交给下游；不保证 nginx 连接解耦 |
| spool 超 `max_buffer_size` | 缓存前缀与触发超限 chunk 输出一次，之后逐 chunk 透传 |
| `X-Accel-Buffering: no` + 路由 on | 该响应逐 chunk 透传 |

## 2. 契约

```yaml
routes:
  - id: api
    path_prefix: /api/
    upstream: origin
    proxy_buffering: true
    max_buffer_size: 1048576
    proxy_set_header:
      - name: host
        value: client_host
      - name: x-tenant
        value: premium
    proxy_del_header: [accept-encoding, x-debug]
```

`proxy_buffering` 默认 `true`; `max_buffer_size` 默认 1 MiB。`false` 与 `X-Accel-Buffering: no` 都使响应逐 chunk 透传。spool 超限时不保留触发 chunk 于 spool,但会与缓存前缀合并并输出一次。实现不承诺 nginx 完整 buffering 的 upstream/client 解耦或临时文件行为。

header value 支持固定字符串,以及 `client_host` 和 `upstream_host` 两个内置值。nico 没有 nginx 的配置层级,每条 route 独立声明,不采用 nginx 全有全无继承语义。

## 3. 结构

| 件 | 归属 | 内容 |
|---|---|---|
| schema | `nico-core` `RawRoute`/`Route`/semantic | response buffering 与 header rewrite 字段 + token、ASCII、framing 名单和 provider 校验 |
| `HttpContext` 缓存 | `nico-http` context | bind_route 缓存 response buffering/max_buffer/改写清单;请求缓冲保留既有插件字段 |
| 响应 spool | `proxy/hooks/body.rs` | body hook 读取 response buffering 决策;扣留 chunks 至 end-of-stream 后整体交给 stages;超限时合并缓存前缀与当前 chunk,然后透传 |
| header 改写 | `proxy/hooks/request.rs` | `upstream_request_filter` 在自动 `Host`/XFF 后应用 set/del 清单;`client_host` 从 context 读取客户端原 Host |
| E2E | `crates/nico-server/tests/http_g4.rs` | route header 覆盖/删除抵达 origin |

## 4. 测试策略

| 层 | 覆盖 | 位置 |
|---|---|---|
| schema | 字段校验矩阵、provider 拒绝、framing 名单 | `nico-core` 文档测试 |
| spool | 缓存、超限不重复/丢失 chunk、超限后停止缓存 | `nico-http` context 单测 |
| headers | set/del/client_host; framing 拒绝 | `nico-core` + E2E |
| 不回归 | 无字段路由零行为变化;LLM 路由不受影响 | 既有套件 |

## 5. 边界

- 磁盘 spool、临时文件生命周期与取消清理不在范围内。
- request buffering 保留 `plugins[].buffers_body` 的现状；Pingora 当前 hook 顺序无法承诺 nginx“完整 body 到达前不发送 upstream head”。
- nico 没有配置层级,不复制 nginx 全有全无 header 继承规则。
- `X-Real-IP`、响应头改写(`add_header`)与 `proxy_pass_request_headers off` 不在范围内。
