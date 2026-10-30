# WP-8：租户物理清理候选枚举安全边界提案

任务 id：`WP-8-tenant-purge-candidate-enumeration-security`。

状态：已批准并完成候选枚举安全边界实施。`WP-8-tenant-purge-runtime` 已获批准并进入实现前核对；核对发现现有
`tenant_purge` 运行角色只能执行固定的 `tenant_purge_batch(bigint, text)` 和
`tenant_purge_finalize(bigint)`，没有直接读取 `tenants` 的权限。因此不能在不扩大安全
边界的情况下实现“自动枚举已过 30 天恢复窗口的 deleted 租户”。本提案只申请补齐候选
枚举的最小安全路径，不申请真实租户删除或生产调度启用。

基线与判据：`docs/plan/proposals/WP-8-tenant-purge-runtime.md` §2.1–§2.2；
`docs/design/spec/05-安全与租户隔离.md` §3.3；
`docs/design/eng/04-CI门禁.md` C16、C17、A8；
`migrations/0056_tenant_purge_finalizer.sql`；
`migrations/0057_tenant_purge_batches.sql`。

## 一、核对事实

当前权限形状如下：

- `tenant_purge` 没有 `tenants` 表的直接 `SELECT`；
- `tenant_purge` 只被授予两个固定函数的 `EXECUTE`；
- `tenant_purge_batch` 的参数是单个租户 id 和固定类别，不能返回候选租户列表；
- `tenant_purge_finalize` 同样要求调用者提供单个租户 id；
- `tenant_purge_batcher` 虽有受限的 `tenants(id,status,deleted_at)` 读取权限，但它是
  `NOLOGIN`，不能作为 CLI 的直接连接角色；
- 让 CLI 直接改用 `tenant_purge_batcher`、`tenant_purge_finalizer`、`tenant_cleanup`、
  `tgm_owner` 或 `postgres` 都会违反现有最小权限边界。

因此，当前不能安全实现无参数自动扫描；也不能把 `SELECT` 权限直接加给
`tenant_purge`，因为这会把表读取权限从固定函数边界扩大到运行进程，并需要同步 C16/C17
和反向测试。

## 二、候选方案与建议

### 2.1 推荐：新增固定候选函数

新增一个只返回候选租户 id 的 `SECURITY DEFINER` 函数，建议签名为：

```text
public.tenant_purge_candidates(bigint)
RETURNS TABLE (tenant_id bigint)
```

建议约束：

- 函数 owner 使用现有 `tenant_purge_batcher`，固定 `search_path = pg_catalog, pg_temp`；
- 只接受正整数 `limit`，固定上限不超过 5000；
- 内部固定查询 `status = 'deleted'`、`deleted_at <= statement_timestamp() - interval '30 days'`，
  按 `id` 升序返回；
- 只返回 `tenant_id`，不返回租户名称、用户 id、状态文本、时间戳或任何业务数据；
- `PUBLIC` 撤销 `EXECUTE`，仅授予 `tenant_purge`；禁止转授；
- 不改变 `tenant_purge_batch`、`tenant_purge_finalize` 的权限和函数正文；
- 在 C17 中增加函数属性、owner、ACL、固定正文、角色关系和返回列检查；
- A8 仅精确放行新增完整签名，不放宽到模式级 `SECURITY DEFINER`。

该方案仍需新增迁移文件，且需要同步 `migrations-test`、审计查询和 `audit-double-pass`，
所以不能在当前运行入口任务内直接实现。

### 2.2 不推荐：给 `tenant_purge` 直接授予列级 SELECT

虽然改动较小，但会让运行角色能够绕过候选函数直接读取所有已授权列，失去“只能通过固定
函数操作”的边界；还需要重新核对泄露面、角色权限和撤销后的失败形状。本方案不采用。

### 2.3 临时退化：强制 CLI 必须传 `--tenant-id`

这可以避免新增权限，但不能满足已批准运行入口提案中的自动扫描与 systemd 日常调度，只能
作为人工运维临时工具，不能冒充完整的运行入口实现。本方案不把它写成 WP-8 自动清理完成。

## 三、实施与验收范围

本提案获批后，只实施候选枚举安全路径：

1. 新增一份前滚迁移和对应 `migrations-test` 文件，不修改 `0056` 或 `0057–0060`；
2. 更新 `C17`、A8 精确放行和权限审计；
3. 增加受限角色测试：只返回到期 `deleted` 租户 id，排除未到期、active、suspended、
   不存在租户及达到上限之外的行；
4. 增加直接表读取、函数重载、错误 limit、`PUBLIC EXECUTE`、owner/角色成员和
   `BYPASSRLS` 反向测试；
5. 运行迁移幂等、SQLx prepare/check、`audit-rls`、`audit-double-pass` 和完整本地门禁；
6. 通过后再回到 `WP-8-tenant-purge-runtime`，实现 CLI 与 systemd，不在本提案内启用生产
   调度或删除真实数据。

## 四、停止条件

如果候选枚举需要返回除租户 id 之外的字段、需要修改现有函数、需要新增通用清理权限、需要
修改 RLS 既有策略或需要改变 WP-8 出口判据，必须重新升级，不在本片扩大范围。

[report]
task: WP-8-tenant-purge-candidate-enumeration-security
path: ok
gates: pass
escalation: docs/plan/proposals/WP-8-tenant-purge-candidate-enumeration-security.md
