# WP-8：租户物理清理限批与共享阻塞重试提案

任务 id：`WP-8-tenant-purge-batches`。

状态：待批准。当前 `WP-8-tenant-physical-delete` 已完成受限根删除终结入口和写入围栏，
但尚未实现租户子表限批清理、共享引用阻塞后的可重跑路径、CLI、调度或生产验收。
本提案只申请实现 PostgreSQL 主库内的受限批处理片，不申请生产启用或真实租户删除。

基线与判据：`docs/plan/proposals/WP-8-tenant-physical-delete.md` §二、§三、§四；
`docs/design/plan/00-里程碑与工期.md` §4.3 WP-8；
`docs/design/spec/03-数据模型.md` §一至§七；
`docs/design/spec/04-数据生命周期与容量.md` §一、§1.1–§1.3；
`docs/design/spec/05-安全与租户隔离.md` §一、§二、§七；
`docs/design/eng/03-测试策略.md` §三；
`docs/design/eng/04-CI门禁.md` A8、C16 及现有租户清理卡口。

## 一、为什么需要单独提案

现有 `public.tenant_purge_finalize(bigint)` 只在租户没有残留时检查共享引用并删除
`tenants` 根行。它不能批量删除子表，不能把一次大级联变成可中断、可重跑的短事务，
也不能在共享事件或在途媒体任务阻塞时报告可核查的阻塞原因。

不能直接复用 `tenant_cleanup`：

- `tenant_cleanup` 面向按套餐期限的普通留存清理，不验证 `status = 'deleted'` 和 30 天宽限期；
- 它拥有既有租户留存类别的删除/脱离权限，扩大用途会混淆权限边界和完成语义；
- 物理删除需要处理身份单列 FK、共享 `canonical_events`、`media_group_members` 和
  `media_relay_jobs`，而普通租户留存路径不拥有根删除语义。

本片因此新增一个固定、受限、以租户为输入的批处理入口，并继续复用已有
`tenant_lifecycle:<tenant_id>` 生命周期锁和事件生命周期共享锁，不创建第二套通用清理框架。

## 二、已核实的外键与删除边界

### 2.1 可按租户删除的私有行

以下关系最终由租户根行的 `ON DELETE CASCADE` 或固定子表删除顺序处理：

1. 规则依赖：`rule_conditions`、`rule_replacements`、`rule_versions`，再删除 `rules`；
2. 规则输入：`keywords`、`source_subscriptions`、`target_channels`；
3. 认证与配置：`web_sessions`、`login_sessions`、`bot_console_sessions`、
   `bot_console_updates`、`tenant_consents`、`tenant_members`、`credential_keys`；
4. 身份附属：`identity_secrets`、`account_leases`，仅针对属于该租户的身份；
5. 租户事件投影与投递：`delivery_logs`、终态且可删除的 `delivery_tasks`、
   `notify_dedup`、`tenant_notifications`、`tenant_events`；
6. 租户审计与租户日志：按既有级联和留存边界处理，不把平台级清理日志伪装成租户日志。

实际实现必须在迁移和受限测试中再次依据数据库约束确认顺序；本列表不是授予任意表名
或任意 SQL 的接口。

### 2.2 必须保留的共享关系

以下对象不能因清理某个租户而被强删、改挂或改写：

- 仍被其他租户 `tenant_events` 引用的 `canonical_events`；
- 被 `media_group_members` 引用的共享事件；
- 状态为 `PENDING`、`DOWNLOADING` 或 `READY` 且仍需处理的 `media_relay_jobs`；
- 全局复用的 `media_archive` 及其对象存储 key；
- 其他租户指向待清租户身份的 `source_subscriptions`、`target_channels`、
  `delivery_tasks`、`bot_console_updates` 或 `tenant_notifications`。

共享阻塞存在时，只删除不会破坏这些关系的私有行，保留完成 FK 所需的 tombstone、
身份、canonical 和必要媒体关系。不得通过修改其他租户数据解除阻塞。

## 三、拟采用的批处理接口

### 3.1 固定入口

新增迁移中的受限 `SECURITY DEFINER` 函数，建议签名为：

```text
public.tenant_purge_batch(bigint, text)
```

其中第一个参数是租户 id，第二个参数是固定类别枚举，不是表名。实现必须：

- 由 `tenant_purge_batcher` NOLOGIN 角色持有；该角色不得是表或 schema owner，
  不得拥有 SUPERUSER、BYPASSRLS、CREATEDB、CREATEROLE、REPLICATION 或角色成员关系；
- 仅允许 `tenant_purge` 登录角色执行，撤销 PUBLIC EXECUTE，不允许转授；
- 固定 `search_path = pg_catalog, pg_temp`，不执行动态 SQL，不接受 cutoff、limit、
  SQL 片段或任意列名；
- 在 READ COMMITTED 下获取 `tenant_lifecycle:<tenant_id>` 排他锁，并设置受限租户上下文；
- 每次调用最多影响 5000 行；每次调用是一个短事务，日志与删除同事务提交；
- 只返回固定结果结构，不返回密文、session 内容、消息正文、对象 key 或其他租户数据。

第二个参数仅允许下列固定类别，具体名称在实现前保持稳定并进入审计：

```text
identity_dependents
rule_dependents
configuration
notifications
terminal_delivery
tenant_events
tenant_audit
```

若某类别的真实 FK 关系要求拆分，必须新增固定类别，而不能把类别退化成任意表名。

### 3.2 批次上限与删除顺序

每个类别的 SQL 必须固定 `LIMIT 5000`，以稳定主键或明确时间/主键复合顺序选取候选，
并在同一事务内删除候选及写入结果日志。建议顺序如下：

1. `identity_dependents`：`identity_secrets`、`account_leases`；
2. `rule_dependents`：`rule_conditions`、`rule_replacements`、`rule_versions`；
3. `configuration`：规则、关键词、来源、目标及控制台/会话/凭据关系；
4. `notifications`：租户通知和通知去重；
5. `terminal_delivery`：先确认无保留中的 `delivery_logs`，再处理终态投递任务；
6. `tenant_events`：只删除租户自己的投影，不删除仍有其他租户引用的 canonical；
7. `tenant_audit`：按既有期限和级联边界处理租户审计/日志；
8. 所有私有残留清除后，再调用现有 `tenant_purge_finalize(bigint)` 尝试根删除。

上述顺序是实现约束，不代表允许批处理函数单独删除根行。根删除仍只能通过现有
`tenant_purge_finalize(bigint)` 完成；批处理函数不得授予 `tenants` DELETE。

`tenant_events` 删除前必须取得既有事件生命周期锁；与共享清理的锁顺序必须统一，
避免形成 `tenant_lifecycle` 与事件锁的反向等待。媒体组和中转任务涉及共享事件时，
只做引用检查，不删除共享对象。

### 3.3 结果码

批处理函数返回固定结果码和行数，至少包括：

- `deleted`：本批完成并提交；
- `empty`：该类别没有可删除候选；
- `locked`：生命周期锁或事件生命周期锁无法取得；
- `blocked_shared_reference`：存在其他租户投影、媒体组或共享 canonical 引用；
- `blocked_inflight`：存在未完成投递或在途媒体中转；
- `not_due`：未满足 `deleted` 状态或 30 天宽限期；
- `not_deleted`：租户状态不是 `deleted` 或租户不存在；
- `failed`：数据库操作或固定日志写入失败，事务回滚。

`locked`、`blocked_shared_reference`、`blocked_inflight`、`not_due`、`not_deleted` 和
`failed` 均不得被 CLI 汇总为成功。`deleted` 仅表示该批提交，不表示租户根行已删除；
只有现有 finalizer 返回 `deleted` 才表示根行已删除。

### 3.4 共享阻塞与 tombstone 重试

批处理过程中：

- 共享引用存在时，保留必要 tombstone、身份、canonical 和媒体关系，记录阻塞类别与计数；
- 不删除其他租户的 `tenant_events`、投递任务、配置或身份关系；
- 不把共享事件改挂平台身份；
- 不恢复租户访问，不保留凭据，不刷新恢复窗口；
- 后续重跑重新检查数据库当前状态，阻塞消失后继续下一批；
- 若阻塞长期存在，结果必须持续可查询并进入告警输入，不能伪造完成。

阻塞日志只能写固定的租户 id、类别、结果、行数和阻塞计数，不写消息正文、身份密文、
session 内容、对象 key 或任意调用者 JSON。

## 四、权限与审计范围

本片命中 `AGENTS.md` §五第 1、3、4 条，必须在批准后才实施。

实施必须新增独立迁移（建议 `0057_tenant_purge_batches.sql`）和对应
`migrations-test` 文件；不得修改已应用的 `0056` 或其他历史迁移。

最小权限要求：

- `tenant_purge_batcher` 只通过固定函数取得批处理所需的列级访问；不给表级宽泛 `DELETE`；
- 不给 `tenant_purge` 或 `tenant_purge_batcher` 根表 `DELETE`、`TRUNCATE`、建对象、角色管理、
  密文读取、session 内容读取或对象 key 读取权限；
- 对每张可删除表明确固定类别、候选列、排序列、批次上限和 RLS/函数上下文；
- 固定日志策略只能接受本片规定的模块、事件、结果码、类别、行数及计数；
- C16 必须继续验证现有 finalizer；新增批处理权限、函数属性、角色关系和日志策略应新增
  独立审计编号（实施前核对编号是否已占用，暂不在提案中硬编码）；
- A8 的 `SECURITY DEFINER` 精确例外只能扩展到本片批准的固定函数，不得放宽为模式级例外。

函数不应通过 `SET ROLE` 或调用者可控对象取得权限；函数 owner、默认权限、PUBLIC
EXECUTE、角色成员和 schema CREATE 权限必须在同一迁移中收口并进入反向测试。

## 五、验收判据

至少覆盖以下场景：

1. 30 天边界、`active`/`suspended`/`deleted`、不存在租户和重复重跑；
2. 每次调用影响不超过 5000 行，批次中断或数据库错误全部回滚；
3. 删除成功日志失败时，删除和日志同时回滚；
4. 同租户并发批次只有一个成功，锁冲突返回 `locked`，不等待外部网络；
5. 事件生命周期锁、共享清理和媒体中转并发时不误删 canonical、媒体组或其他租户投影；
6. 其他租户引用待清身份、canonical 或媒体关系时返回明确阻塞，不修改引用方数据；
7. `PENDING`、`DOWNLOADING`、`READY` 中转任务分别验证在途/共享边界；
8. 阻塞消失后重跑可继续，持续阻塞不报告完成；
9. finalizer 只在私有残留归零且共享引用解除后删除根行，根删除失败可安全重试；
10. 受限连接不能直接删除表行、读取密文/session/消息正文/对象 key、TRUNCATE、建对象或提升角色；
11. 迁移重入、真实删除级联、SQLx prepare/check、专项审计双遍和完整本地门禁通过；
12. 完成后检查 `df -h`、本片专属 `target/` 以及任务新增 Docker 容器、匿名卷和网络，
    不删除正在使用的 Compose 具名卷。

测试必须以受限角色连接为主；不得用 owner、超级用户或测试恒真策略替代隔离验证。

## 六、未包含内容与停止点

本片不实现以下内容：

- `purge-deleted-tenants` CLI、systemd timer、生产告警和生产凭据配置；
- 真实生产租户删除或连接生产数据库；
- Redis、备份、运行日志、Telegram 已投递消息、对象存储副本的删除；
- 改写既有 FK、改变 `tenant_consents` 的级联语义或新增归档表；
- 任意通用表清理框架或新的第三方依赖。

本提案获批准后，在隔离 worktree 实施；完整本地门禁通过后再申请单提交、推送和向
`dev` 开 PR。PR CI 和合入仍分别等待维护人裁决。本片通过不等于 WP-8 整体、G3/G4
或生产部署通过。

[report]
task: WP-8-tenant-purge-batches
path: ok
gates: pass
escalation: -
