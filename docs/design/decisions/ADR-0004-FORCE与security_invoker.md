# ADR-0004：RLS 一律 FORCE，视图一律 `security_invoker`，禁用 `SECURITY DEFINER`

状态：已定案

## 决定

- 每张启用 RLS 的表同时 `ENABLE` 与 `FORCE ROW LEVEL SECURITY`
- 所有视图带 `security_invoker = true`
- **全面禁用 `SECURITY DEFINER` 函数**

## 背景

这三条是同一次实测的三个结论，写成一条 ADR 是因为它们不能分开采纳。

`ENABLE` 只对非 owner 生效。表 owner 查自己的表时 RLS 直接不适用 ——
这意味着任何时候有人用 owner 身份走了一次业务路径，隔离就不存在，
**而且没有任何迹象**。`FORCE` 把 owner 也纳入策略。

> FORCE 才是承重的那个开关。`ENABLE` 单独存在给人的安全感是假的。

加上 FORCE 之后出现第二个现象：原先「owner 路径能看到全部数据」的漏洞
变成了「返回 0 行」。于是：

- 普通视图（默认 `security_invoker = off`，以视图 owner 身份取基表）→ 0 行
- `SECURITY DEFINER` 函数 → 0 行
- `security_invoker = true` 的视图 → 正常
- 普通 SECURITY INVOKER 函数 → 正常

也就是说 FORCE 把这两类构造从「安全漏洞」变成了「**安全但功能坏掉**」。
它不会泄漏，但它会静默地什么都查不到 —— 这种 bug 在开发期极难定位，
因为代码看起来完全正确。与其留着这个陷阱，不如整条禁掉。

`SECURITY DEFINER` 还有第二个问题：它是唯一能合法绕过 FORCE 的构造。
留一个后门在库里，等于给「以后为了方便加一个」留了先例。

## 代价

- 不能用 `SECURITY DEFINER` 实现跨租户的平台运维查询。
  替代方案是 `platform_ops` 角色 + 显式审计，路径更长。
- `security_invoker` 的写法有两个坑，落在静态断言 A8 上：
  视图对基表的依赖记在 `pg_rewrite` 上而不是视图的 oid 上
  （按 `pg_depend.objid = v.oid` 查会返回 0 行，断言静默永绿）；
  `security_invoker = on` 存成 `{security_invoker=on}` 而 `= true` 存成
  `{security_invoker=true}`，用数组包含匹配会对合规视图误报。
  必须用 `pg_options_to_table(...)::bool`。
- FORCE 之下 owner 自己也受策略约束，种子数据与迁移的顺序要额外注意
  （PG 15+ 的 `public` schema 权限变化会叠加这个问题）。
- 「0 行」这个失效形状要求反向测试断言**行数**而不是断言无异常。

## 相关

[[ADR-0003]]、[[ADR-0016]]。
落地在 `spec/05-安全与租户隔离.md`、`eng/04-CI门禁.md`（A8）、`eng/03-测试策略.md`（R6）。
