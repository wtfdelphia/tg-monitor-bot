# WP-8-media-cleanup-logs：媒体清理平台日志闭环

状态：2026-10-27 已获批准并在隔离工作树完成实现；定向测试、迁移幂等、审计、双遍反证和完整本地门禁均已通过。
当前未提交、未推送、未开 PR，生产环境未连接；验收记录见
`docs/plan/evidence/WP-8-media-cleanup-logs-2026-10-27.md`。

## 任务与升级原因

任务 id：`WP-8-media-cleanup-logs`。

本片补齐 `docs/design/spec/04-数据生命周期与容量.md` §一关于媒体清理的
`system_logs` 闭环。现有媒体清理已经能删除对象、清空档案 key、删除到期档案，
并清理 `staging/` 孤儿对象，但每一步只写 `tracing`；实现中的 `ponytail:` 注释
已经记录这一缺口。

本片命中 `AGENTS.md` §五第 1、3 条，实施前必须升级并取得批准：

- 需要新增独立数据库角色及迁移；
- 需要新增 `system_logs` 的平台级 RLS 写入策略、GRANT 和审计覆盖。

上述范围已获批准；本文件同时记录实施边界和验收结果，不扩大到其他清理角色。

## 实施前事实核对

- `crates/app/src/media_cleanup.rs` 的清理运行在 fanout 的 `app_user` 数据库连接上，
  当前分三步写 `event="cleanup.done"` 的 `tracing` 日志：
  `media_archive.object`、`media_archive`、`staging`。
- `system_logs` 已启用并强制 RLS。`app_user` 现有策略只允许写当前
  `app.tenant_id` 的租户日志，不能写 `tenant_id IS NULL` 的平台日志。
- `media_archive` 是跨租户按 `content_hash` 复用的共享表，按现有设计不启用 RLS；
  当前业务角色通过 GRANT 访问，不能把租户上下文硬套到这张表。
- `shared_cleanup` 已有平台日志写权限，但策略只接受
  `module='shared_retention'` 且 `table='canonical_events'`，同时该角色还拥有共享
  原始事件清理所需的删除权限，扩大它的职责会违反最小权限边界。
- `docs/design/eng/01-配置与Secret.md` 已把共享清理连接串限定为
  `tgm cleanup-shared`，不能把同一凭据静默注入常驻 fanout。

## 推荐方案：独立 `media_cleanup` 角色

新增仅供 fanout 内部媒体清理使用的数据库角色 `media_cleanup`，由 fanout 为媒体
清理单独建立受限连接池；普通 fanout 查询继续使用 `app_user`。媒体清理不再尝试
通过 `app_user` 写平台日志，也不复用 `shared_cleanup`。

### 数据库权限边界

新增幂等迁移（正式迁移与 `migrations-test` 对应迁移保持一致）：

- 创建 `media_cleanup LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE
  NOREPLICATION`，只授予当前数据库 `CONNECT`；
- 授予 `media_archive` 所需的最小列级读取权限，以及对象清理所需的
  `UPDATE (object_key, object_expires_at)` 和限定删除权限；
- 授予 `system_logs` 的列级 `INSERT (tenant_id, level, module, event, context)`
  与 `system_logs_id_seq` 的 `USAGE`；
- 新增 `system_logs` 的 `FOR INSERT TO media_cleanup` 策略，只允许：
  - `tenant_id IS NULL`；
  - `level='INFO'`、`module='media_retention'`、`event='cleanup.done'`；
  - `context` 只有 `table`、`rows`、`elapsed_ms` 三个字段；
  - `table` 只能是 `media_archive.object`、`media_archive` 或 `staging`；
  - `rows` 与 `elapsed_ms` 为非负整数，`rows` 不超过单批上限。

`media_archive` 当前的三步查询和并发保护必须以受限角色重跑；不把
`media_cleanup` 加入任何租户表的 RLS 策略，也不授予 `canonical_events`、
`tenant_events`、`delivery_tasks`、`audit_logs` 或其他清理角色权限。

### 应用连接链路

- 新增只供 fanout 的 `TGM_DB_URL_MEDIA_CLEANUP`；配置预检要求它直接以
  `media_cleanup` 登录，并拒绝混入 owner、业务、其他 cleanup、KMS、Telegram
  和对象存储之外的凭据。
- `FanoutWorker` 保留 `app_user` 主池，再接收一个可选的媒体清理池；只有媒体清理
  使用该池，扇出与业务查询不改变。
- 生产启动时 fanout 必须构造媒体清理池；缺失或连接失败要在启动阶段明确失败，
  不能退回 `app_user`，也不能把日志降级成“看起来成功”的 tracing-only 路径。
- 对象存储凭据仍沿用既有 fanout 专用静态 S3 凭据，不把数据库凭据或 AWS 默认
  凭据链混入媒体日志方案。

### 日志事务与失败语义

- 每个实际清理批次写一条 `media_retention/cleanup.done`；无候选时各阶段仍写一条零行记录。
  保持现有脱敏形状，不写 object key、content hash、租户标识或 S3 错误正文。
- 每条日志的 `rows` 只记录该阶段已经成功完成的删除数：对象删除成功且数据库
  条件清空成功才计入 `media_archive.object`；档案物理删除和 staging 对象删除
  分别计入自己的阶段。
- 对象删除失败时保持档案 key，不计入成功数；当前清理结果继续保留失败计数，
  下一轮重试。
- 平台日志写入失败时，媒体清理本轮返回错误；fanout 记录失败并在下一个周期
  重试，不输出该阶段的成功完成结论。日志失败不能被 tracing 覆盖成成功。
- 日志与对应数据库删除应保持同一数据库事务边界。对象存储删除属于外部系统，
  仍遵守现有“先删对象、成功后再清空 key”的顺序；对象已删而日志/数据库更新
  失败时，下一轮依靠幂等条件继续收敛，不能重新写入对象 key。

## 不采用的方案

### 直接放宽 `app_user` 的平台日志策略

不采用。`app_user` 是常驻业务角色，允许它写 `tenant_id IS NULL` 会让任一业务
路径获得平台日志伪造能力；即使把 module 和 table 收窄，也会把平台清理审计边界
与租户业务连接混在一起。

### 扩大 `shared_cleanup`

不采用。该角色已经拥有共享原始事件删除权限，媒体清理只需要 `media_archive`
与固定平台日志写入。复用会扩大职责和配置耦合，无法保持最小权限。

### 继续只写 tracing

不采用。`spec/04` 要求每次清理写入 `system_logs`；tracing 不是可查询的保留审计
记录，也无法支持后续容量校准。

## 实施结果

1. 已新增正式迁移和测试迁移，创建角色、GRANT、`system_logs` 平台写入策略。
2. 已更新 `crates/db/src/audit/queries.sql` 及对应安全审计断言，覆盖新角色的登录
   属性、对象权限、日志列权限和序列权限。
3. 已增加配置校验、fanout 媒体清理连接池和启动接线；删除
   `media_cleanup.rs` 中仅写 tracing 的 `ponytail:` 缺口。
4. 已把媒体清理测试切换为 `media_cleanup` 受限角色，并补充日志断言。
5. 已补齐测试证据，记录分阶段日志、成功计数、失败不清 key、重复执行幂等、日志
   失败回滚/重试以及敏感字段不泄露。
6. 已运行定向静态检查、生产迁移、在线 SQLx check、媒体清理测试、`audit-rls`、
   `audit-double-pass` 和完整本地门禁；完整结果见验收证据文件。

## 验收判据

- 每个实际清理批次产生一条固定模块和固定事件的 `system_logs` 记录；无候选阶段产生零行记录。
- 日志 `rows` 与各阶段成功删除数一致，失败对象不计数且档案 key 保留。
- 重复执行不重复删除、不重建档案、不产生错误的成功计数。
- 日志写入失败时本轮失败，不伪造 `cleanup.done` 成功结论。
- 平台日志不包含 object key、hash、租户 id 或外部存储错误正文。
- `app_user`、`shared_cleanup` 和未知角色不能写 `media_retention` 平台日志。
- `media_cleanup` 不能访问共享原始事件、租户投影、投递任务和审计日志。
- 受限角色、空库、隔离反证及完整本地门禁全部通过；最终以 PR CI 为裁决。

## 发布边界

独立 `media_cleanup` 角色 + fanout 独立连接池方案已获批准并实施。完整门禁通过后，
仍需按仓库发布纪律另行授权提交、推送和开 PR；本片不代表整个 WP-8 通过或生产部署获批。
