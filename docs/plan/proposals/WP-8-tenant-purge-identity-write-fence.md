# WP-8：注销租户身份写入围栏补充提案

任务 id：`WP-8-tenant-physical-delete`。

状态：2026-10-28 维护人已批准。追加迁移 `0055_identity_lifecycle_write_fence.sql`、受限角色测试和 C15 身份围栏部分，
专项与完整本地门禁通过；本提案只补充终结函数提案 §3.3 的写入围栏，不追加第二个 definer 例外。

判据出处：`../../design/plan/00-里程碑与工期.md` §4.3 WP-8；
`WP-8-tenant-physical-delete.md` §3.1–§3.3、§四；
`WP-8-tenant-purge-finalizer-security.md` §3.3、§3.4；
`../../design/decisions/ADR-0039-恢复窗口与删除完成范围分离.md`。
实测见 `../evidence/WP-8-注销租户身份写入围栏反证-2026-10-28.md`。

## 一、需要补什么

同意记录最终清理采用“先清空全部私有身份，再限批清成员与同意记录”的已批顺序。
但是现有受限 `app_user` 可以绕过应用 helper，为已经注销且无身份的租户重新插入身份与 canonical。
跨批次仅释放生命周期锁再重查，不能使已经删除的同意记录恢复；事后发现共享引用并报告阻塞也来不及。

已有 Bot/MTProto 创建入口通过 `tenant_write_tx` 取共享锁并检查 active，保留并复用这条链路。
缺的是数据库写入约束，不是再写一套业务服务、终结状态或永久删除登记表。

## 二、建议的最小范围

- 追加幂等迁移，在 `identities` 增加固定 `SECURITY INVOKER`、VOLATILE 的 BEFORE INSERT / UPDATE OF tenant_id 触发器。
  对 `app_user` 写入的非空租户身份取既有 `tenant_lifecycle:<id>` 事务级共享锁，
  随后用新语句检查租户仍 active，否则以固定脱敏错误拒绝。不用现有外层 INSERT 的快照代替取锁后复查。
- 强制这条路径的 READ COMMITTED 前提，与应用 helper 的隔离级别要求一致；
  在长快照、无租户上下文或上下文不匹配时失败，不把 GUC 本身当成状态证明。
- 不授新表写权限、不修改原来的租户隔离策略、不允许 app_user 更新身份归属；
  `tenant_id IS NULL` 平台身份仍由既有权限/RLS 控制，不借此给业务角色创建共享身份的能力。
  不以 SECURITY DEFINER、owner、BYPASSRLS 或恒真策略读取租户状态。
- 测试管理和迁移身份仅用于种子与 DDL，不作为隔离断言主体；
  审计确认所有实际运行角色的身份 INSERT/归属 UPDATE 权限，任何未经列明的可写角色使 C15 报红。
- 将固定触发器的目录属性、启用状态、invoker 属性、函数体与权限纳入已批准的 C15，
  同时保留最终阶段每批的空身份/排空/期限复查和原有最终根残留检查。
- 不新增表或状态列、不改已有迁移、不改 FK/级联、不改变同意记录期限或 tombstone 规则。
  终结函数精确例外的 ADR 与文档按既有批准继续实施，其他 definer 仍受 A8 禁令约束。

## 三、必须证明的性质

1. 活动租户 Bot/MTProto 身份正常写入；suspended、deleted、已删除根行、不匹配租户上下文和长快照拒绝写入。
   真实 `app_user` 连接验证直接 SQL，不只测调用应用 helper；业务 API 行为仍须回归。
2. 写入先取得共享锁时，注销/清理排他锁等待其事务结束，随后能看见并处理已经提交的身份。
   清理/注销先取得排他锁时，身份插入等待后用新语句看见非 active 或根行不存在并拒绝。
   两种顺序均用两个连接和明确屏障，不用顺序测试冒充并发。
3. 清空身份后到下一最终批次期间，受限角色不能重建身份并为其挂新事件；
   原有身份/canonical/job 的残留继续阻止开始同意记录清理，不靠根级联扫尾。
4. 测试 search_path/临时对象、触发器禁用/替换、额外可写运行角色和归属更新越权；
   C15 双遍逐项破坏并恢复，不能因 RLS/权限不可见把非空当空。
5. 迁移重入、受限角色回归、SQLx prepare、A8/C15 与完整门禁通过后才报告本片实现完成；
   围栏没有通过前，物理删除与同意记录最终清理保持不可用。

若 PostgreSQL 在该触发器形状下不能证明“取锁后新语句可见”，不得只加一个 status 表达式就继续，
应报告失败并重新裁决。此前探针只证明基线缺口；修复后已由受限角色探针和两个连接的实际锁等待测试验证，
记录见 `../evidence/WP-8-注销租户身份写入围栏实施-2026-10-28.md`。
平台语义参考 PostgreSQL 17 官方的函数快照和触发器可见性说明：
`https://www.postgresql.org/docs/17/xfunc-volatility.html`、
`https://www.postgresql.org/docs/17/trigger-datachanges.html`。
这些说明支持选择 VOLATILE，但不替代本仓的两个连接并发验证。

## 四、当前验证与停止点

基线身份重建探针 rc=2，确认缺口；维护人已批准第二节的最小身份写入围栏及 C15/测试范围。
新迁移、专项并发测试、既有凭据/生命周期回归和完整本地门禁通过，修复后的身份直写探针 rc=0。
迁移重入、SQLx prepare/check、A8/C15 及审计双遍通过，双遍收尾目录快照恢复。
C15 当前只覆盖已实施的身份围栏，不把尚未创建的 purge/finalizer 权限记为已审计。
此前物理清理、终结函数和留存语义的批准保持有效，不需要重批；未完成对应测试前不启用物理删除。

不连接生产、不执行真实租户删除；提交、推送、PR 和合并仍由用户分别授权。

[report]
task: WP-8-tenant-physical-delete
path: ok
gates: pass
escalation: docs/plan/proposals/WP-8-tenant-purge-identity-write-fence.md
