# h2c 配置生效取证（手工，curl）

> 目的：证明 `http.h2c` 不是空开关——同端口 H1/H2 都由它决定，而且默认（`false`）确实拒绝明文 H2。
> 自动化测试只能断言 `HttpProxy.server_options.h2c` 被设置（见 `crates/gateway-http/src/service/tests.rs`）；
> 真正的协议协商需要 H2 客户端，PRD 不允许为此引入测试依赖，因此这一条以可复现的手工取证记录。

## 环境

- 二进制：`cargo build -p gateway-server`（debug）
- 资源文档：空路由的合法文档（`providers: []` / `upstreams: []` / `routes: []`），因此任何请求都走 `404 route_not_found`，
  不依赖上游可用
- bootstrap 关键字段：`http.listen: 127.0.0.1:<空闲端口>`、`http.threads: 2`、
  `http.grace_period_ms: 1000`、`http.graceful_shutdown_ms: 5000`
- 启动：`RUST_LOG=info ./target/debug/gateway-server --bootstrap <fixture>`
- 客户端：`curl 8.x`（本机自带 H2 支持）

## `h2c: true`

```
$ curl -sS -o /dev/null -w 'HTTP/%{http_version} %{http_code}\n' --http1.1  http://127.0.0.1:<port>/api/
HTTP/1.1 404
$ curl -sS -o /dev/null -w 'HTTP/%{http_version} %{http_code}\n' --http2-prior-knowledge http://127.0.0.1:<port>/api/
HTTP/2 404
```

同端口同时接受 H1 与明文 H2，并且两者都走到同一套路由/错误映射（`404 route_not_found`）。

404 响应体（同时证明 `trace_id` 回显调用方的 `x-request-id`、响应体是稳定 JSON 形状）：

```
$ curl -sS --http2-prior-knowledge -H 'x-request-id: evidence-1' http://127.0.0.1:<port>/api/
{"error":{"code":"route_not_found","message":"no route matches the request","trace_id":"evidence-1"}}
```

## `h2c: false`（同一份文档，仅改这一个布尔值）

```
$ curl -sS -o /dev/null -w 'HTTP/%{http_version} %{http_code}\n' --http1.1  http://127.0.0.1:<port>/api/
HTTP/1.1 404
$ curl -sS -o /dev/null -w 'HTTP/%{http_version} %{http_code}\n' --http2-prior-knowledge http://127.0.0.1:<port>/api/
curl: (16) Remote peer returned unexpected data while we expected SETTINGS frame.
        Perhaps, peer does not support HTTP/2 properly.
curl exit=16
```

H1 照常服务，明文 H2 在协商阶段被拒绝——正是 `h2c` 默认 `false`、必须显式开启的预期行为。

## 结论

- `http.h2c` 真实决定同端口是否接受明文 H2；默认关闭，开启需运维显式写 `true`。
- 两种协议命中同一个监听、同一套路由与错误映射，不存在"另开一个 H2 监听"的第二条路径。
- 复现方式：把上面的 fixture 换成任意 `config/resources.yaml` 内容，端口取空闲端口即可。
