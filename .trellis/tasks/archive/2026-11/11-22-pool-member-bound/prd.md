# 路由池成员数上界与 llm ip_hash 测试

## Goal

把上一个任务新增的 `routes[].upstreams` 池接入既有的 `max_pool_members` 上界
(`validate.rs::check_pool_counts` 此前只数 llm 池),并为 llm 侧 ip_hash 的粘性补上测试——
client 地址穿线在上一个任务实现,但无测试钉住。

## Background

`SchemaBounds::max_pool_members`(默认 8)是路由池的既有资源界:llm 池由
`validate.rs::check_pool_counts` 强制,且是路由尝试上限(`attempt_ceiling`)的因子。上一任务
新增的 upstream 池写进了同一个 `Route` 却逃过了该检查——一份文档可以为一条路由声明任意多的
upstream 成员。

`PoolStrategy::IpHash` 随上一任务进入共享枚举;llm 侧的 `build_pool_picker` 有分支、client 地址
穿到了 `PoolOrder::order` 的 `View`,但没有任何测试证明"同调用方稳定首选"。

## Requirements

1. `check_pool_counts` 同时数 `route.upstream_pool()`,kind 为 `routes[].upstreams`,同一
   `max_pool_members` 界。
2. schema 测试:upstream 池成员数超界被拒(默认界 8,写 9 个成员);合法数量通过。
3. llm 测试:ip_hash 池对同一调用方稳定首选(注入 `PoolOrder` + IpHash picker)。
4. 无既有行为变化:llm 池的界不变;默认界内文档不受影响。

## Acceptance Criteria

- [ ] AC1:upstream 池成员数超 `max_pool_members` 被拒,错误 kind 为 `routes[].upstreams`;
      llm 池的既有界不回归。
- [ ] AC2:llm ip_hash 同调用方稳定首选有测试。
- [ ] AC3:门禁全绿(fmt/clippy/test/doc/deny/预算/matrix)。

## Notes

- 小任务:一个守卫扩展 + 两个测试;PRD-only,无 design.md。
