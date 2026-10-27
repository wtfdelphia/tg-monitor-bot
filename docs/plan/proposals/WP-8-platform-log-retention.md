# WP-8-platform-log-retention：平台系统日志到期清理

状态：2026-10-27 已获维护人批准，隔离工作树实施与完整本地门禁通过；维护人同日授权本批合为一个提交、推送并开 PR 到 `dev`。
验收证据见 `../evidence/WP-8-platform-log-retention-2026-10-27.md`；合并及生产启用另行授权。

任务 id：`WP-8-platform-log-retention`。

基线：PR #57 合并提交 `97c449bbb2d0ce93642f47310a51df017434ce31`。
该 PR CI 与合并后 push CI 均已通过，不代替本片验收。

## 一、问题与复用核查

本片对应 `docs/design/plan/00-里程碑与工期.md` §4.3 WP-8 的到期数据清理出口，
期限与批次纪律沿用 `docs/design/spec/04-数据生命周期与容量.md` §一、§1.1。

实施前核对结果：

- `system_logs` 同时容纳租户日志和 `tenant_id IS NULL` 的平台日志；生命周期规格未把平台行
  排除在该表留存策略之外。
- `crates/db/src/tenant_retention.rs` 的 `Category::SystemLogs` 在候选与删除处均要求
  `tenant_id = $1`，不会匹配平台行。不能删掉这个条件来补平台清理。
- `auth_cleanup`、`shared_cleanup`、`media_cleanup` 分别负责既有清理与固定平台日志写入，
  均没有平台系统日志到期删除路径。PR #57 补的是日志写入，不是日志留存。
- `platform_ops` 用于平台运维查询和租户管理，不应把其较宽的访问面交给后台清理命令。
- 可以复用现有清理的配置校验、受限角色预检、限批、事务、取消和 systemd 样例形状；
  不需要新表、依赖或通用清理框架。

选择先补这片，是为了在继续扩大生命周期流程前补齐已落库平台日志的回收路径。
租户注销仍是 WP-8 的另一条未完成出口，不因本片替代或延期免验；同意文本和举报受理边界另行批准。

## 二、拟实施范围

### 2.1 独立受限角色与迁移

追加幂等正式迁移及对应测试迁移，创建 `platform_cleanup`；不修改已应用迁移。
角色要求直接登录、非超级用户、无 BYPASSRLS、无建库/建角色/复制权限，无角色成员关系、
无对象所有权及转授权。生产迁移不写入密码，本地初始化与测试凭据另行配置。

只授予当前数据库 CONNECT、public schema USAGE，以及以下对象权限：

- `system_logs` 的 `SELECT (tenant_id, id, created_at)` 和 DELETE；
- 固定清理日志需要的列级 `INSERT (tenant_id, level, module, event, context)`；
- `system_logs_id_seq` 的 USAGE。

保留 ENABLE/FORCE RLS。新 SELECT/DELETE 策略同时要求 `tenant_id IS NULL` 和达到现行留存期限；
应用查询也显式检查相同条件。即使人为设置租户上下文，也不能读删任意租户日志。
角色不可读取 `context`、正文或其他表，不可 UPDATE/TRUNCATE 日志，不可指定日志 id 或 created_at。

INSERT 策略只接受平台行、`INFO`、`platform_retention/cleanup.done`，
context 恰为 `table`、`rows`、`elapsed_ms`；table 固定为 `system_logs`，
行数符合现行单批上限，计数和耗时为非负整数。不按 module 豁免旧清理日志，
清理自身的历史日志也遵守同一保留规则，新日志不会被本批当成过期候选。

新增 `C13` 精确审计角色属性、成员关系、对象/列/序列权限及转授边界，
同步 `eng/04` 与审计双遍；双遍额外授予 `system_logs.context` 读取权限时应仅由 `C13` 报红，
撤销后目录快照恢复。策略语义另由受限连接反向测试证明，不能只靠静态审计。

### 2.2 独立命令与配置

新增 `tgm cleanup-platform` 和独立 `TGM_DB_URL_PLATFORM_CLEANUP`。
复用现有清理配置检查，只允许 profile、该连接串及可选日志配置；拒绝 owner、业务、
其他清理、KMS、Telegram、对象存储凭据和 `DATABASE_URL` 混入。
普通服务和其他清理命令同样拒绝持有新连接串，不复用 `platform_ops` 或常驻业务池。

启动检查 session_user/current_user 均为 `platform_cleanup` 并核对安全属性。
数据库失败、错误角色、日志失败和取消都不得输出“清理成功”；对外只输出固定脱敏错误，
不泄漏连接串、数据库对象详情或日志内容。

### 2.3 批次、并发与失败语义

- 按数据库 cutoff、完整 `(created_at, id)` 键排序与现行批次上限删除；
  恰好 cutoff 算到期，下一微秒保留，删除语句再次核对时间及 NULL 租户条件。
- 同一清理类别使用事务级 advisory lock，重叠执行不重复清理；报告 overlap，不冒充完成。
- 每批删除与固定平台清理日志同事务提交；日志失败整批回滚，包括零行批次。
  rows 只统计本批实际删除行，不把日志新增行或其他租户行计入。
- 批间等待并释放事务锁，SIGTERM 取消未提交批次、回滚删除与日志；已提交批次保留，重跑幂等。
- 使用 READ COMMITTED；不支持的隔离级别直接拒绝，不依赖数据库默认配置。

### 2.4 部署样例与文档

增加独立系统用户、EnvironmentFile 的 systemd service/timer 样例，频率引用现行生命周期规格。
同步配置清单、角色初始化、生命周期实施说明与部署手册；不改变现行期限、不扩展平台运维 API。
安装样例、临时 user timer 验证与生产启用分开记账，本片不自动安装或启用生产调度。

## 三、验收判据

- 受限角色实测只删除到期平台日志，保留近期平台行和各状态租户的全部租户日志；
  不读取日志 context，不访问会话、凭据、租户投影、事件、任务或其他表。
- 反向验证错误 tenant/module/event、额外 context 字段、负数/越界行数、伪造时间、
  UPDATE/TRUNCATE、错角色及多余/缺失/可转授权限；拒绝都实际命中预期边界。
- 精确 cutoff、超单批候选、重复执行、并发插入、真实行锁等待后资格变化、重叠、
  日志失败回滚及取消后重跑均有可执行检查；固定日志计数与实际删除一致。
- 实际放宽到期策略后，近期平台行保护用例必须报红；移除租户 NULL 限制后，
  租户日志哨兵用例必须报红，反证后源码/策略恢复。
- 隔离空库走生产路径迁移、角色校验、真实清理与审计；临时 timer 实际触发新命令。
- 运行在线 SQLx prepare/check、迁移可重入、工作区测试、审计和双遍，最终完整本地门禁通过；
  skipped/ignored 项与生产待验项分别记录，不计作通过，合入仍须 PR CI 裁决。
- 验收结束核对并清理本任务构建缓存、临时容器、卷和网络；不动已有 Compose 栈和具名卷。

## 四、升级与实施边界

本片命中 `AGENTS.md` §五第 1、3 条：新增迁移、角色/GRANT/RLS、审计及其双遍反证。
上述数据库、CLI、配置隔离、调度样例和测试范围已获批准，按本提案实施，不扩大生产权限。

不新建表，不修改旧迁移，不实现租户注销、同意文本或举报接口，不读取生产凭据，
不连接生产数据库、Telegram 或 AWS，不宣布 WP-8、G3、G4 或生产部署通过。
维护人已授权本批合为一个提交、推送并开 PR 到 `dev`；合入仍须 PR CI 裁决与维护人授权。

[report]
task: WP-8-platform-log-retention
path: ok
gates: pass
escalation: docs/plan/proposals/WP-8-platform-log-retention.md
