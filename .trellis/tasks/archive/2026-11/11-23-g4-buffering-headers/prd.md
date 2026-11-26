# G4 缓冲与 header 控制(nginx 对齐)

## Goal

对齐 nginx 当前 Pingora hook 接缝可支持的 G4 子集:**有界响应内存缓冲**与**路由级请求头改写**。
nginx 的完整响应缓冲(含临时文件与连接生命周期解耦)和 request buffering 不在本里程碑承诺范围。

## Background

nginx 语义(官方文档核实):

- `proxy_buffering`(默认 **on**):nginx 将响应缓冲到内存和临时文件,并让上游读取与慢客户端消费
  解耦;`proxy_buffering off` 同步透传(流式/SSE);后端可用 `X-Accel-Buffering: no` 逐响应覆盖。
- nginx `proxy_request_buffering`(默认 **on**)要求在完整 body 到达前不建立/发送 upstream 请求；
  Pingora 0.9.1 的 `upstream_request_filter` 已经写出 upstream request head，之后才进入
  `request_body_filter`，所以 nico 保留 `plugins[].buffers_body` 的真实语义（head 先发、body
  chunk 扣留到 end-of-stream），不新增一个语义不等价的路由开关。
- `proxy_set_header`:客户端原 `Host`/`Connection` 不透传,重定义为 `$proxy_host`(proxy_pass
  里的名字);**全有全无继承**(location 写一条就丢掉上层全部);值设空串 = 抑制该头;
  `proxy_pass_request_headers off` 整体不透传。

nico 现状与差距:`upstream_response_body_filter` 可将响应 chunk 扣留到 end-of-stream 后整体交给下游,
但只使用有界内存,不落磁盘;超限后透传。它不承诺 nginx 临时文件语义或上游连接生命周期与客户端发送
完全解耦。请求侧 `buffers_body` 保持 head 先发的现有语义;`Host`/任意头改写此前只有插件接口,
没有路由级配置。

## Requirements

### R1|有界响应缓冲:`routes[].proxy_buffering`(默认 on,nginx 子集)

- **on**(默认):上游响应体经**有界内存 spool** 扣留到响应结束,再作为一个整体交给下游;
  `max_buffer_size` 默认 1 MiB,路由级可调。此行为不保证上游连接的释放时点与 nginx 相同。
- 超过 `max_buffer_size`:超限 chunk 不进入 spool;已缓存前缀与超限 chunk 合并后输出一次,后续 chunk
  逐个透传,不报错。M1 不落磁盘,不声称等价于 nginx 的 temp-file 溢出。
- **off**:逐 chunk 透传(流式/SSE/事件流路由显式声明)。
- `X-Accel-Buffering: no` 响应头逐响应覆盖 on;off 路由的 no 无效果。
- LLM 路由不参与(convert 已整体缓冲,事件流透传是语义);`on_response_body` 插件阶段在 spool
  抽取完成后对完整 body 运行一次。
- `max_buffer_size` 限制 spool 累积量;触发超限的完整 chunk 会形成短时峰值,随后切换为透传。

### R2|请求缓冲：保留现有 `buffers_body`，不伪称 nginx 对齐

完整 request buffering 需要在 Pingora 写 upstream request head 之前控制数据流；当前 hook 接缝无法
做到，因此本里程碑不暴露 `proxy_request_buffering`。现有 `plugins[].buffers_body` 继续保留：
upstream head 先发，body chunk 被网关扣留，end-of-stream 后整体喂给插件/转发。需要 nginx 同等
“收齐 body 才建立 upstream 请求”时，另立底层数据流任务。

### R3|路由级 header 控制:`proxy_set_header` / `proxy_del_header`

- `routes[].proxy_set_header: [{name, value}]`、`routes[].proxy_del_header: [name]`;value 支持
  固定字符串(不支持 nginx 变量——`$host` 场景用内置枚举 `client_host` 表达客户端原 Host,
  `upstream_host` 表达现默认)。
- 应用点:`upstream_request_filter`,在 `X-Forwarded-*` 与自动 `Host` **之后**——路由级改写
  可覆盖自动值,`client_host` 枚举提供 nginx `$http_host` 的等价物。
- 凭证头与 hop-by-hop 保护不变:`authorization`/`cookie` 可被覆盖(路由级显式意图),但
  hop-by-hop 名单仍由框架清除;`content-length`/`transfer-encoding` 由框架管理,写入即发布
  失败。名称必须是合法 token,值必须可见 ASCII(发布期校验)。
- 不继承 nginx 的"全有全无"规则:nico 没有层级,每条路由独立声明;自动行为(`Host`、
  `X-Forwarded-*`)先应用,改写后应用——语义上等价于 nginx 的"默认 + location 覆盖"且少一个坑。
- WASM 插件与本特性无关:插件契约的 `set_request_header` 已存在,WASM 模块 M1 不开放 body/头
  改写导出(§11 边界)。

### R4|范围外

- 磁盘 spool(临时文件、清理、与 generation 的交互)——`max_buffer_size` 超限降级透传;
  `proxy_busy_buffers_size`/`proxy_temp_file_write_size` 等精细旋钮;`X-Accel-*` 其余头;
  `proxy_pass_request_headers off`;响应头改写(`add_header`)。

## Acceptance Criteria

- [ ] AC1:schema——`proxy_buffering` 布尔与 `max_buffer_size` 校验;
      `proxy_set_header` 名称/值校验(token、可见 ASCII、禁 content-length/transfer-encoding)、
      `proxy_del_header` 同;provider 路由拒绝(改写是 upstream 域词汇)。
- [ ] AC2:响应缓冲 on 时 body chunks 只在 end-of-stream 后交给响应 stages;off 与
      `X-Accel-Buffering: no` 保持逐 chunk 透传;超限时累计内容(含触发 chunk)输出一次并切换透传。
- [ ] AC3:现有 `buffers_body` 的 C9 语义保留并文档化；不声称实现 nginx 的 request buffering。
- [ ] AC4(E2E):`proxy_set_header` 覆盖 Host/自定义头抵达 origin;`proxy_del_header` 移除;
      空值抑制;`client_host` 枚举取客户端原 Host;`content-length` 写入被发布拒绝。
- [ ] AC5:`X-Accel-Buffering: no` 覆盖 on 路由;off 路由不受影响。
- [ ] AC6:门禁全绿;LLM 路由与无配置路由零行为变化。

## Notes

- 实现顺序:先 R3(纯配置化,复用插件已有的改写落点),再实现并验证 R1 的有界内存 spool；R2
  保留为明确的兼容差异,不实现语义不等价的路由开关。
- nginx 迁移对照表补 `proxy_buffering`/`proxy_request_buffering`/`proxy_set_header`/
  `proxy_del_header` 四行。
