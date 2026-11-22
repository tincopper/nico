# traffic upstream 路由的 URI 前缀改写(G1 最小形状)

## Goal

traffic upstream 路由能把请求路径中**已匹配的 `path_prefix`** 替换为配置的值——nginx
`location /api/ { proxy_pass http://up/internal/; }` 的最小形状。改写之外的路径逐字节保留
(query、编码、尾斜杠),改写只作用于转发,不参与匹配。

## Background

现状:`/api/` 路由把原路径原样转给上游(`/api/foo` 到达上游仍是 `/api/foo`),且前缀不剥离
("路由前缀也不会被剥离"——README 既有立场)。nginx 迁移审查 G1 把 URI 替换列为高影响缺口:
现有 `/api/ → /internal/` 的 origin 无法用公开配置表达,只能由上游自己消化前缀或部署一层代理。

本任务取审查建议的"最小需要的 rewrite":前缀替换。精确/正则/虚拟主机分流、按 body 分流等
更丰富的路由不在本次,真实需求出现再评估。

## Requirements

### R1|schema:`routes[].rewrite_prefix`

```yaml
routes:
  - id: api
    path_prefix: /api/
    upstream: internal
    rewrite_prefix: /internal/   # 可选;缺省 = 原样转发
```

- 替换已匹配的 `path_prefix`:转发的路径 = `rewrite_prefix + 原路径去掉 path_prefix 前缀的部分`。
  必须是 `/` 开头的路径——替换是**字面前缀交换**,空值会把 `/api/items` 变成 `items`(不是合法的
  origin-form 目标),剥离整个前缀写 `/`;不得包含 `?`、`#`、空白或控制字符——这些在发布期拒绝,
  转发期的 URI 重组因此不会失败。
- 仅对 **upstream 路由**合法;`provider` 路由拒绝(provider 的路径由适配器所有,改写不得
  静默用于转换链——审查报告的约束)。池化 upstream 路由同样可用(改写是路由层事实,与哪个
  成员服务无关)。

### R2|转发语义

- 改写在 `upstream_request_filter` 应用(每轮一次),从**客户端原始路径**(`ctx.path()`)重算
  ——对 failover 的重试轮天然幂等,不会二次改写。
- query 逐字节保留:`?` 之后的字符串原样跟随新路径;路径的百分比编码不解码不重编码。
- `Host`、`X-Forwarded-*` 的既有规则不变(改写只动路径)。

### R3|匹配与转发的次序

- 改写发生在路由匹配**之后**:匹配用的仍是 `path_prefix` 与客户端原路径,改写只改变转发的
  目标路径。路由匹配、优先级、插件链全部不受影响。
- 无 `rewrite_prefix` 的路由零行为变化(既有测试不回归)。

### R4|范围外

- 正则改写、多段替换、按方法/header 条件改写;provider 路由(含 passthrough)的路径改写;
  响应路径改写;按 body 分流。真实场景出现按 findings 的验收矩阵另立任务。

## Acceptance Criteria

- [ ] AC1:schema 正负例——非法值(不以 `/` 开头且非空、含 `?`/空白)、provider 路由拒绝;
      合法值进入文档,池化路由同样携带。
- [ ] AC2(E2E):`/api/` → `/internal/` 的真实转发——上游收到 `/internal/items`;
      query(`?a=1&b=%20`)与深层路径(`/api/x/y`)逐字节保留;无改写字段的路由原样转发。
- [ ] AC3(E2E):failover 路由(重试轮)改写恰好一次——上游收到的是改写后路径,无叠加。
- [ ] AC4:门禁全绿(五基线 + matrix + 预算)。

## Notes

- 一个交付单元;nginx 迁移对照补 `proxy_pass 带 URI` 一行。
