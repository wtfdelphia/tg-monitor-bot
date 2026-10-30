# WP-8：租户物理清理批处理列权限缺口提案

任务 id：`WP-8-tenant-purge-permission-gap`。

状态：已批准，实施完成。该提案由 `WP-8-tenant-purge-runtime` 的专项 CLI 测试发现；批准后新增最小安全迁移，未修改已应用迁移。

## 一、发现

`public.tenant_purge_batch(bigint, text)` 的 `configuration` 分支在删除
`source_subscriptions` 前读取 `public.tenant_events.source_ref`，并读取
`public.rules.target_ref` 判断目标频道是否仍被规则引用。
现有 `migrations/0057_tenant_purge_batches.sql` 只向 `tenant_purge_batcher` 授予
`tenant_events(tenant_id, id, canonical_event_id)` 和 `rules(tenant_id, id)` 的列级 SELECT，
没有授予 `source_ref` 或 `target_ref`。

因此，使用正确 `tenant_purge` 登录角色运行 `tgm purge-deleted-tenants` 时，在进入
`configuration` 类别后会收到 PostgreSQL `42501 permission denied for table tenant_events`，
随后在补齐该列并继续验收时还会暴露 `rules.target_ref` 的同类缺口；即使该租户没有实际
事件或规则引用也会失败。结果来自真实 `wp8-batches` Compose PostgreSQL 和新的 CLI 集成测试，
不是连接角色错误或测试假绿。

## 二、拟议最小变更

1. 新增只增不改的安全迁移，为 `tenant_purge_batcher` 最小补授
   `SELECT (source_ref)` on `public.tenant_events` 和 `SELECT (target_ref)` on `public.rules`。
2. 在 `migrations-test/` 同步同一迁移，保持 SQLx 隔离库与生产迁移形状一致。
3. 不增加 `tenant_purge` 的表级直接读取权限；不增加其他列、UPDATE、INSERT、TRUNCATE
   或角色成员关系。
4. 复核 `tenant_purge_batch` 全部七类 SQL 的列引用与现有列级 GRANT，确认不存在同类缺口；
   如发现额外缺口，必须逐项列出并保持最小权限，不以 `GRANT SELECT ON` 代替。
5. 更新 C17/专项安全测试，使两列权限和 `configuration` 类别可运行性有明确反向断言。

## 三、验收判据

- `tenant_purge` 仍不能直接读取 `tenant_events` 或任意敏感列，只能执行批准的
  `tenant_purge_candidates`、`tenant_purge_batch`、`tenant_purge_finalize` 函数。
- `tenant_purge_batcher` 只新增 `tenant_events.source_ref` 和 `rules.target_ref` 的 SELECT 列权限；角色属性、成员关系、
  RLS、函数 owner 和函数执行者不变。
- 空到期租户可按固定七类顺序完成批处理，并由 finalizer 返回 `deleted` 后删除根行。
- 共享引用、在途任务、锁冲突、未到期和失败结果仍保持原有非零/可重跑语义。
- `crates/db/tests/tenant_purge.rs`、`bins/tgm/tests/tenant_purge.rs`、C17、迁移幂等、
  `audit-rls`、`audit-double-pass` 和完整本地门禁通过。
- 不修改已应用的 `0056`、`0057`、`0058`、`0059`、`0060` 或 `0061` 文件。

## 四、当前运行入口实现的边界

`WP-8-tenant-purge-runtime` 已在隔离工作树落下 CLI、应用编排、数据库调用封装、专用配置校验、
systemd service/timer 和集成测试。本提案补充的 `0062` 迁移已同步生产与测试迁移源；专项数据库测试
和权限精确性测试已通过。完整 workspace 门禁在当前环境受到既有测试端口/角色基线阻断，运行入口
本身不因此自动宣布 WP-8 通过。

[report]
task: WP-8-tenant-purge-permission-gap
path: ok
gates: pass
escalation: docs/plan/proposals/WP-8-tenant-purge-permission-gap.md
