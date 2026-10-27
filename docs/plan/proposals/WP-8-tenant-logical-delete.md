# WP-8：租户逻辑注销与凭据销毁

任务 id：`WP-8-tenant-logical-delete`。

状态：2026-10-27，WP-8 首片“暂停、恢复与执行围栏”已由 PR #59 合入 `dev`；本提案申请第二片实施批准，尚未修改生产代码、迁移或数据库权限。

基线：`dev` 合并提交 `aab345b4808b8a07c51e49678553b6873b6b0e5e`。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-8；
`docs/design/spec/01-产品需求.md` §七；`docs/design/spec/04-数据生命周期与容量.md` §一、§1.3；
`docs/design/spec/05-安全与租户隔离.md` §三、§七；
`docs/design/spec/06-合规与凭据责任.md` §二；
`docs/design/decisions/ADR-0029-授权凭证不留存.md`；
`docs/design/decisions/ADR-0038-暂停围栏与在途结果分离.md`。

## 一、为什么现在做第二片

首片已经建立 `active ↔ suspended` 的停止执行前置：暂停租户不再接受业务写入或新的外部执行，后台租约与未开始投递被收敛，已运行 worker 和在途外部结果按真实边界处理。

但现有状态机仍没有“主动注销”的执行路径：

- `tenants.status` 已有 `deleted` 值，但没有可靠的 `deleted_at` 起算时间，也没有从 `suspended` 进入 `deleted` 的受限命令；
- `identity_secrets` 与每租户 `credential_keys` 仍可能保留可用凭据；
- 恢复入口当前只允许 `suspended → active`，不能让已经逻辑注销的租户恢复；
- 既有租户清理角色只处理到期临时记录，不能删除凭据或改变租户生命周期；
- 直接物理删除根租户仍属于第三片，必须保留共享 `canonical_events`、`tenant_events`、投递和外键保护。

本片的目标是：**逻辑注销立即使租户不可业务读写，立即销毁租户凭据，并保留可恢复的业务数据至宽限期；不做根租户物理删除。**

## 二、范围与不做项

### 2.1 本片实现

1. **可靠删除起算时间**
   - 追加幂等迁移，为 `tenants` 增加 `deleted_at`；`active`/`suspended` 必须为空，`deleted` 必须非空。
   - 存量 `deleted` 行首次迁移时保守使用迁移时数据库时间回填；重复迁移不刷新已有时间。
   - `suspended → deleted` 只允许通过独立受限生命周期命令，不能由 `app_user`、`tenant_cleanup`、Web 或 Bot 修改。

2. **逻辑注销状态机**
   - 只允许 `suspended → deleted`；不允许 `active → deleted`，调用方必须先完成首片暂停与排空。
   - 逻辑注销幂等：重复执行不刷新 `deleted_at`，但必须再次确认凭据已销毁。
   - `deleted` 不允许恢复；本片不提供“撤销注销”或 `deleted → active/suspended`。
   - 注销事务写入固定 `source=system` 审计；审计上下文只记录状态和固定计数，不记录密文、session、Token、API hash 或对象 key。

3. **凭据立即销毁**
   - 在同一生命周期事务中删除该租户所有 `identity_secrets`，并删除该租户 `credential_keys`。
   - 删除前获取租户生命周期排他锁，阻止新的凭据写入、QR/2FA 完成回写、Bot session 回写及 worker 续租；网络 KMS/Telegram 调用不在事务锁内等待。
   - 删除结果必须按行数和剩余查询核对；无法确认零行时整批回滚并非零退出，不把“已标记 deleted”伪装成“凭据已销毁”。
   - 对租户内所有身份标记 `unauthorized` 或其他既定终态，使旧 worker 即使持有过期快照也不能重新执行；不删除平台共享身份。
   - `credential_keys` 只在确认该租户不再有身份凭据后删除；不触碰其他租户或 NULL `tenant_id` 的平台身份。

4. **读取与写入边界**
   - `deleted` 租户不允许业务读取、业务写入、投递、采集、目标探测、登录完成回写和 session 落库；只允许独立生命周期命令读取必要状态与固定计数。
   - 已有 Web/Bot 会话不能继续访问租户业务数据；本片可在同一事务删除租户会话或使会话查询统一失败，但不得扩大 `auth_lookup` 到凭据表。
   - 不改变共享原始事件的全局归属；逻辑注销不删除 `canonical_events`、媒体对象、其他租户投影或平台演示身份。

5. **宽限期准备**
   - 为后续第三片提供 `deleted_at` 作为宽限期起点；本片不自动把 `deleted` 转为物理删除。
   - 业务数据、审计/系统记录、规则和同意记录按现有规格保留，直到第三片按依赖顺序处理；不在本片重新定义保留期。

### 2.2 明确不做

- 不实现 `deleted` 租户的物理删除、跨表归零、共享事件回收或对象存储清理；这些属于第三片，需另行提案和批准。
- 不新增管理 HTTP API、控制台页面、租户自助注册或法律文本；本片只提供独立运维命令和底层状态边界。
- 不修改已应用迁移，不连接生产数据库，不读取真实 Telegram/AWS 凭据，不执行真实租户注销。
- 不改变 `tenant_consents` 的保留决策；它仍按 ADR-0029 随第三片物理删除级联消失。
- 不删除或修改平台共享身份、其他租户数据、共享 `canonical_events` 或跨租户投递记录。

## 三、复用与实现形状

- 复用首片 `tenant_lifecycle` 角色、生命周期 advisory lock、`WorkerGuard`、`tenant_write_tx` 和连接预检；不创建第二个生命周期框架。
- 复用 `identity_secrets` 的身份归属查询和 `credential_keys` 的租户粒度；删除路径不能读取密文，只取 identity id、状态和计数。
- 复用既有 `ON DELETE CASCADE` 作为第三片删除机制，但本片不触发根租户 DELETE；共享原始事件的 `NO ACTION` 防线保持不变。
- 追加迁移只做 `deleted_at`、触发器/约束、生命周期角色的最小列级权限和必要索引；迁移文件自身幂等，测试迁移按仓库 symlink 纪律维护。
- `tgm tenant-lifecycle delete --tenant-id <id>` 使用生命周期专用连接串；命令必须拒绝 `DATABASE_URL`、owner、业务、Telegram、KMS、对象存储凭据混入。
- 删除流程采用 READ COMMITTED：生命周期排他锁 → 锁定租户行 → 再次确认 `suspended`、无有效租约/未排空 worker → 标记 `deleted` → 删除凭据/会话 → 固定审计与完成日志同事务提交。任一步失败回滚。
- 与首片一致，生命周期事务不持有锁等待 KMS、Telegram、Redis 或对象存储网络调用；旧外部请求的不确定结果按 ADR-0038 记录，不能靠删凭据宣称网络连接已关闭。

## 四、权限与安全升级

本片命中 `AGENTS.md` §五第 1、3、4 条，实施前必须获得明确批准：

- 新增迁移和 `deleted_at` 状态约束；
- 修改租户生命周期角色的列级 `UPDATE`/`DELETE` 权限与 RLS 策略；
- 新增或调整审计断言（包括删除路径的固定日志形状）；
- 改变凭据销毁与认证会话的信任边界。

最小权限要求：

- `tenant_lifecycle` 只能读取租户状态、删除租户自有凭据/会话所需的身份键列和固定计数；不得 `SELECT` 密文、API hash、session id、消息正文或对象 key。
- `tenant_lifecycle` 不得拥有根租户 `DELETE`、身份 `DELETE`、成员关系 `DELETE`、`TRUNCATE`、角色成员、建库、建角色、复制或 `BYPASSRLS` 权限；本片只删除凭据行并改变受限状态。
- 任何列级授权、策略或触发器必须由 C14 与新增专项断言逐项检查；把 `USING (true)` 或 owner 连接当作隔离验证一律不计数。
- 删除凭据与状态变更必须同一事务提交；日志只允许 `tenant_id`、固定事件名、状态和计数，不允许可注入自然人 actor 或任意 JSON。

## 五、验收判据

获批后至少覆盖：

1. `active → deleted`、`suspended → active`、`deleted → active/suspended`、不存在租户和重复注销均按固定错误/幂等语义处理；`deleted_at` 首次设置后不刷新。
2. 存量 `deleted` 迁移回填可重复，`active/suspended/deleted` 与 `deleted_at` 约束在真实 PostgreSQL 上生效。
3. 注销前必须完成首片排空；存在有效 lease、`PENDING/FAILED/SENDING`、不确定在途或运行 worker 时拒绝注销，不伪造成功。
4. 使用受限 `tenant_lifecycle` 连接确认：能销毁本租户密文与 DEK，但不能读取密文、API hash、session 内容，不能触碰其他租户或平台共享身份。
5. 注销后真实 Web cookie、Bot 成员入口、QR/2FA 完成、凭据写回、Bot session 回写、采集、投递、目标探测和租约续期均失败关闭；恢复入口不能复活 deleted 租户。
6. 并发验证注销/凭据写入、注销/QR 完成、注销/session 回写、注销/worker 续租、注销/恢复；移除生命周期锁或状态复查后对应反向测试必须按预期报红。
7. 凭据删除逐行可核对，事务日志失败、权限不足、中断和重复执行均不产生“已注销但凭据未销毁”的假成功；回滚后可安全重跑。
8. 其他 active 租户、平台共享身份、共享 `canonical_events`、其他租户投递任务和现有会话不受影响；deleted 租户的业务数据仍保留供第三片宽限期清理。
9. 迁移幂等、SQLx prepare/check、定向测试、全量本地门禁、审计双遍和目录快照通过；ignored 真机项单独记录，不冒充生产验收。
10. 门禁结束后检查磁盘、进程引用和 Docker 临时残留；只清理本片专属构建缓存，不销毁现有 Compose 数据卷。

## 六、交付与停止点

获批后在隔离工作树实施，按一个任务一个 commit；完成本地门禁后再申请提交、推送和 PR。
PR 目标为 `dev`，不自动合并。PR CI 是最终裁决，CI 通过后仍需维护人明确批准合入。

本提案不宣布 WP-8、G3/G4、生产部署或租户删除出口通过；只申请第二片“逻辑注销与凭据销毁”实现权限。

[report]
task: WP-8-tenant-logical-delete
path: escalated
gates: not-run
escalation: docs/plan/proposals/WP-8-tenant-logical-delete.md
