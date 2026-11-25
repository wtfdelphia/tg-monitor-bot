# G4：生产历史指纹重算的只读访问设计提案

任务 id：`G4-production-fingerprint-readonly-access`。

状态：提案阶段，等待维护人批准。

## 一、当前缺口

G4 生产历史指纹重算授权模板列出了所需表、字段、关联路径和只输出聚合计数的要求。本次只读核对 `cloud/dev@994ab82a2f941ec50bc37146fbfb16d5ff6758c1` 的迁移、GRANT、RLS 策略和已有测试查询后，没有找到既满足最小读取范围又能安全覆盖这条路径的现有登录角色。

- `tgm_owner` 是 `NOLOGIN` owner，不作为扫描登录身份。
- `app_user` 对 `canonical_events` 有表级 DML。该共享表没有 `tenant_id` 和 RLS，业务 SQL 纪律也要求经 `tenant_events` 投影查询，故不能当作只读扫描角色。
- `tenant_cleanup` 在相关表上有 `DELETE`，且列权限不足以完成重算。
- `shared_cleanup` 对 `canonical_events` 有 `DELETE`，只读列是清理元数据，不能执行指纹重算。
- `tenant_lifecycle` 可读有限 `delivery_tasks` 状态并更新生命周期字段，权限范围不足且包含写权限。
- `platform_ops` 与 `auth_lookup` 的权限域不含本扫描所需业务数据列。
- `tenant_purge`、`tenant_purge_batcher` 和 `tenant_purge_finalizer` 带有删除、函数执行或清理权限。
- 这三张表的业务租户隔离策略面向 `app_user`，另有针对特定维护角色的清理/生命周期策略；这些策略不覆盖本扫描所需的完整读取关联路径。新角色不能只凭租户过滤条件就继承该边界。

因此当前 `production_role_and_privileges=not-provided`，`production_data_scan=not-authorized`。不得将扫描模板或普通 SQL 的 `READ ONLY` 事务误写成数据库身份和行级范围已经受限。

判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G4；`docs/design/eng/03-测试策略.md` §三；`docs/design/eng/04-CI门禁.md` A8/A9；`docs/plan/evidence/G4生产历史指纹重算范围授权模板-2026-11-23.md`；`migrations/0001_roles_and_schema.sql`、`migrations/0010_rls_policies.sql`、`migrations/0011_grants.sql`、`migrations/0029_tenant_retention.sql`、`migrations/0063_g4_notify_dedup.sql`。

## 二、建议方案

采用一次性、范围裁剪的隔离工作副本，不向生产数据库添加角色、GRANT、函数或视图。

1. 先由责任方单独批准生产备份读取、隔离恢复目标、环境隔离、有效窗口、操作者、停止/回滚联系人和清理责任。该批准与 G4 扫描范围批准分别记录。
2. 将已批准的备份恢复到无生产网络连接、无 Telegram/KMS/S3/Redis 外呼能力的临时隔离环境。先完成角色、迁移、RLS 和恢复身份核验。
3. 由隔离环境中的获批管理员按扫描申请范围建立一次性裁剪副本。副本只含 `notify_dedup` SENT 候选、与其关联的 DONE 投递任务、对应的租户事件与所需 canonical event 行，并遵守批准的租户、业务时间、截止时间、过期记录、行数和字段范围。
4. 在裁剪副本上使用临时扫描身份。审批方案必须明确 LOGIN 执行身份与最小权限组角色的关系、`SET ROLE` 路径、成员关系和失效/销毁方式。可采用短期 LOGIN 身份继承一个 `NOLOGIN` 读取角色，但该成员关系只能存在于隔离副本；执行身份须 `NOSUPERUSER`、`NOBYPASSRLS`、无写入/DDL权限，且不能切换到 owner、业务或清理角色。副本仅保留批准范围的数据；对 `canonical_events` 的范围控制依赖裁剪副本内容与获批核验，不能声称该表有 tenant RLS。该临时身份不得获得生产连接权限。
5. 扫描程序在内存中重算 `tgm:l3:v1` 指纹，只输出 `matched`、`mismatched`、`not_recomputable` 等聚合计数及批准的原因类别。不得输出原指纹、消息正文、关键词、sender id、真实租户标识或可还原内容的数据。
6. 扫描结束后停止进程，销毁临时凭据、裁剪副本、恢复库和中间文件，逐项记录清理结果与受控报告摘要。生产备份的原保留策略不因本次扫描而改变。

该路径仍会在受控隔离处理阶段读取获批的生产备份数据。因此必须先完成备份读取/隔离恢复授权、具体扫描授权和数据处理范围确认；它不授予生产服务、生产库实时查询、timer 或真实租户清理权限。

## 三、严格范围

每次扫描的签字申请至少绑定：

- 精确的候选/稳定版本、源码提交、扫描工具 SHA-256 和依赖锁文件；
- 生产备份引用和校验值、隔离恢复节点、网络隔离证明；
- 租户范围、业务时间窗口、查询截至时间、是否包含过期 dedup 记录；
- 表及列白名单、最大行数/批次、最长运行时间、并发数和负载上限；
- `sender_id`、`matched_keywords`、`text`、`caption` 是否可在隔离执行内存中读取；
- 输出仅为聚合计数的格式、存储位置、保留期限和销毁责任人；
- 执行、复核、紧急停止、回滚和恢复副本销毁责任；
- 有效窗口及批准记录编号。

任一范围字段缺失、角色权限与批准不符、目标出现生产网络、超出行数/时间/负载上限、输出包含正文或原始指纹、停止联系人不可达时，立即停止。不得扩大范围或改用 owner 身份继续。

## 四、审批分界

本提案批准后，可在非生产样例上实现并测试裁剪器、临时只读角色、扫描输出和销毁脚本，运行仓库要求的完整门禁。实际读取生产备份、生产数据扫描和目标机恢复各需针对精确对象、范围与窗口的独立授权。

本提案不批准生产备份访问、生产恢复、生产扫描、外部告警、生产 timer、真实租户清理、稳定发布或 Telegram 操作。G4 在真实范围批准和扫描结果核验前保持 `partial`。

## 五、验收与停止条件

- 单元及隔离集成测试证明只扫描批准范围、上限命中后停止，且输出只有批准的聚合字段。
- 用反例证明租户范围、时间边界、过期记录开关、重复任务关联和脱敏字段处理符合申请内容。
- 隔离扫描执行身份与可选 `NOLOGIN` 读取角色的成员关系、切换路径和失效方式有测试证明；执行身份无写入/DDL/函数执行权限，不能连接生产数据库，也不能切换到 owner 或业务/清理角色。
- 检查临时副本与结果销毁流程，失败时保留最小脱敏审计信息。
- `scripts/ai-driver/ai-gates.sh` 和 PR CI 必需检查通过。

若实现要求新增生产角色、迁移、GRANT、安全函数、审计 SQL 白名单或修改安全断言，需将精确数据库变更和 A8/A9 影响作为独立安全实现方案再次审批；本提案本身不实施这些变更。

```text
[report]
task: G4-production-fingerprint-readonly-access
path: escalated
gates: not-run
escalation: docs/plan/proposals/G4-production-fingerprint-readonly-access.md
```
