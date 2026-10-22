# 测试迁移

这里复用生产迁移 `0001`–`0011`，不重复维护 SQL；`0012`/`0013` 不在测试迁移源内，
因为 SQLx 测试连接使用受限的 `tgm_test`，不能创建 event trigger。模板库由
`scripts/init/01-roles.sql` 预置同一套 `ensure_rls` 触发器，SQLx 创建临时库时会继承它。

`0014_test_support.sql` 只给 `tgm_test` 测试库权限与测试管理策略，不进入生产迁移，
也不改变 `app_user`、`auth_lookup` 或 `platform_ops` 的权限面。

`0025_test_support_notifications.sql` 同理：0014 只覆盖它运行时已存在的表，
0024 新建的 `tenant_notifications` 照 0014 的形状给 `tgm_test` 补权限与测试管理策略。
