# WP-8：租户事件与投递留存

任务 id：`WP-8-event-retention`。

状态：已批准实施。本片在隔离工作树实现中；未提交、未推送、未开 PR，也未连接生产库。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-8，
`docs/design/spec/04-数据生命周期与容量.md` §一、§1.1、§1.2，
`docs/design/spec/03-数据模型.md` §五、§六，
`docs/design/spec/05-安全与租户隔离.md` 的运行时权限纪律，
`docs/design/eng/03-测试策略.md` §三及租户留存附加回归，
`docs/design/eng/04-CI门禁.md` 的 RLS 审计与逐项反证。

## 一、基线与范围

基线为 PR #52 合入后的 `dev`：`b31179fc77f31d4fe1576fe35880f4c79e01a6aa`。
合并后 push CI run `40197138138` 已通过，`static`、`database`、RLS 静态审计和审计断言双遍均为成功。

前片已经实现并合入：

- `WP-8-auth-retention`：鉴权会话留存清理；
- `WP-8-tenant-retention`：租户日志与临时记录留存；
- `WP-8-rule-version-retention`：规则版本留存及规则读取到入队之间的并发保护。

本片只处理租户级事件和投递记录的生命周期：

- `tenant_events` 的 free/pro 分档留存；
- `delivery_tasks` 与 `delivery_logs` 在事件删除后的关系；
- 投递任务从入队、认领、发送、重试、恢复到终态期间的清理保护；
- 无关联终态任务的限期清理；
- 清理与投递状态变更之间的真实并发顺序、批量、日志和重跑。

本片明确不处理：

- `canonical_events`、`media_group_members`、`media_relay_jobs` 和 `media_archive` 的共享事件/媒体回收；
- `tenant_notifications` 的 SENT/DEAD 留存期限；
- 租户注销、同意凭证、身份凭据销毁和举报入口。

共享事件必须等本片删除 `tenant_events` 后，再由独立分片按最长租户引用和媒体组引用回收；不把跨租户共享对象与租户级级联放进同一片。

## 二、现状与必须解决的冲突

当前外键链为：

```text
tenant_events.canonical_event_id
  -> canonical_events(id) ON DELETE CASCADE

delivery_tasks.(tenant_id, tenant_event_id)
  -> tenant_events.(tenant_id, id) ON DELETE CASCADE

delivery_logs.task_id
  -> delivery_tasks(id) ON DELETE CASCADE
```

因此直接按 `tenant_events.created_at` 删除会同时删除投递任务和投递日志，而规格要求：

- `tenant_events`：free 7 天、pro 30 天；
- `delivery_logs`：90 天；
- 无关联 `DEAD` 任务：30 天。

如果不先改变关系形状，事件留存到期会提前抹掉仍在 90 天窗口内的 `delivery_logs`。仅把删除分成多条 SQL 不能解决这个外键语义冲突，也不能证明投递中的任务没有被清理。

当前 `delivery` 已有以下状态链路，必须复用而不是另造一条清理模型：

- `PENDING`/`FAILED` 由 `FOR UPDATE SKIP LOCKED` 认领并转为 `SENDING`；
- `SENDING` 由 `updated_at` 超时恢复为 `PENDING`；
- 外部 Telegram 调用结束后，同一事务写 `DONE` 或失败状态及 `delivery_logs`；
- `DEAD`/`FAILED` 可由人工重试接口重新置为 `PENDING`。

任何清理实现都不能把 `PENDING`、`SENDING`、`FAILED` 当作普通历史行，也不能仅用进程内顺序调用代替两个数据库连接的并发验证。

## 三、建议的关系与留存方案

### 3.1 事件删除不再级联删除投递任务

新增追加迁移，保留 `tenant_events` 的租户复合外键，但把删除动作改为只将子表的 `tenant_event_id` 置空：

```text
delivery_tasks.(tenant_id, tenant_event_id)
  -> tenant_events.(tenant_id, id) ON DELETE SET NULL (tenant_event_id)
```

`tenant_id` 仍为 `NOT NULL`，不能使用会同时清空复合键两列的普通 `SET NULL` 形状。迁移必须在 PostgreSQL 17 本地环境验证，保持只增不改、可重入，并确认现有数据不存在违反新约束的行。

这样处理后：

- 删除事件不会删除 `delivery_tasks`；
- `delivery_logs` 不会因事件删除而提前级联消失；
- 已冻结的投递 payload、随机 id 和发送结果仍可按各自期限留存；
- `tenant_event_id IS NULL` 成为明确的「事件已过期/已脱钩」状态，而不是隐式丢失历史。

若目标 PostgreSQL 版本不接受列级 `SET NULL` 语法，必须停在迁移验证处重新升级，不能退回到普通 `ON DELETE CASCADE`，也不能用 owner 连接在应用层伪造同等约束。

### 3.2 租户事件清理

独立清理类别复用 `tenant_cleanup` 和现有 `tgm cleanup-tenant --category` 入口，不新增队列框架或常驻业务权限。

每个租户用同一事务取得数据库 cutoff，并按完整键排序、每批不超过既有 `BATCH_LIMIT` 删除：

- `created_at < cutoff - 7 days`（free）；
- `created_at < cutoff - 30 days`（pro）；
- `created_at` 恰好位于 cutoff 边界的行保留；
- active、suspended、deleted 三种租户状态都参与，不因状态跳过；
- 未知 plan 直接报错，不按 free 回退；
- 每批删除与脱敏的 `tenant_retention/cleanup.done` 在同一事务提交；
- 批间保留既有等待，SIGTERM 只回滚当前未提交批次，后续可重跑。

删除候选必须在事务内重新检查关联任务：

- 只要存在同一事件的 `PENDING`、`SENDING` 或 `FAILED` 任务，就跳过该事件；
- `DONE`、`CANCELLED`、`DEAD` 属于终态，可在外键置空保护下删除事件；
- 没有投递任务的到期事件直接删除；
- 事件删除完成后，下一片再根据 `canonical_events` 的跨租户引用和媒体组引用回收共享对象。

跳过在途事件不是永久保留：任务进入终态后由下一轮清理继续处理。若任务长期卡在 `SENDING`，先走现有超时恢复路径；本片不通过删除事件来掩盖卡住的投递。

### 3.3 投递任务和投递日志清理

`delivery_logs` 继续按 `created_at` 保留 90 天，沿用现有日志清理类别。事件删除不再触发其级联删除，因此日志窗口独立成立。

为避免 `delivery_tasks` 因外键改为 `SET NULL` 后无限增长，新增明确的脱钩终态规则：

- 脱钩的 `DONE`/`CANCELLED` 任务：从 `updated_at` 或其最后一条投递日志的较晚时间起保留 90 天；
- 脱钩的 `DEAD` 任务：无投递日志时保留 30 天；有投递日志时至少保留到最后一条日志超过 90 天，不能通过删任务提前级联删除日志；
- `PENDING`、`SENDING`、`FAILED`：不由历史清理删除；事件也因在途保护暂不删除；
- 任务删除采用完整键和限批，只有终态且满足上述期限才可删除；
- 任务详情所需的冻结 payload 不向 `tenant_cleanup` 授权，清理角色只读状态、时间和归属列，并仅获 `tenant_event_id` 的脱钩更新权限；
- 删除任务前，数据库事务重新确认状态仍为终态，并以日志最新时间重新计算 cutoff。

无关联 `DEAD` 任务的 30 天规则在本片中不被扩大为「所有无 event 的任务均可删」。`DONE`、`CANCELLED` 和带投递日志的 `DEAD` 必须服从日志 90 天约束。

### 3.4 并发协议

清理不能只依赖 `tenant_events` 的年龄谓词。建议统一以下锁顺序：

1. 清理事务取得租户事件清理的排他 advisory lock；
2. 按 `tenant_event_id` 顺序锁定候选事件；
3. 按 `delivery_tasks.id` 顺序锁定该事件的任务并重新读取状态；
4. 发现 `PENDING`、`SENDING` 或 `FAILED` 即回滚该候选的删除并处理下一批；
5. 只有确认没有在途任务，才删除事件，让 FK 将 `tenant_event_id` 置空；
6. 终态任务的独立清理在同一类别锁下重新读取日志最新时间后执行。

投递侧所有会改变任务状态的入口必须遵守相反方向不冲突的共享锁/行锁协议：

- claim 在将任务改为 `SENDING` 前先取得对应事件的共享保护并锁任务；
- stale recovery、retry、complete、failure 都不能绕过该保护；
- 外部 Telegram 请求不在数据库锁事务内执行；发送期间由 `SENDING` 状态阻止历史清理；
- 发送结束回写与清理对同一任务的竞争必须按影响行数和终态重新核对，不能把「更新 0 行」当作成功。

锁只覆盖本仓生产入队、投递和清理入口；不声称能约束任意超级用户或绕过 repo 的直接 SQL。验收必须搜索所有生产调用方，确认没有绕过协议的状态更新。

## 四、权限、迁移与文档范围

命中新增迁移、安全面和执行记录范围，本片获批前不实现。

预计实现范围：

- 新迁移：调整 `delivery_tasks` 到 `tenant_events` 的删除动作；补事件/任务清理所需索引、角色列权限和 RLS 策略；
- `crates/db/src/tenant_retention.rs`：新增事件、脱钩任务类别及真实 cutoff、批量、状态、日志保护 SQL；
- `crates/app/src/tenant_retention.rs`、CLI：暴露固定类别、停止与调度样例；
- `crates/db/src/repo.rs`：补齐投递 claim/retry/recovery/complete/failure 的共享锁或等价行锁协议；
- `crates/db/src/audit/`、`docs/design/eng/04-CI门禁.md`：扩展 `C9` 精确白名单，并加入漏授、多授、列级转授反证；
- `crates/db/tests/`、`crates/app/tests/`、`bins/tgm/tests/`：加入留存边界、状态保护、日志级联和真实并发测试；
- `docs/design/spec/04-数据生命周期与容量.md`：登记事件删除关系、脱钩任务期限和 90 天日志冲突的解决口径；
- `docs/design/spec/03-数据模型.md`、必要的运维配置和本片证据文档；
- SQL 改动后使用生产迁移路径并重跑 `cargo sqlx prepare --workspace`，不删除已有 `.sqlx/` 元数据规避检查。

不改已应用迁移，不把 `canonical_events`、媒体回收、通知留存或租户注销混入本片。

## 五、验收判据

获批后至少应覆盖以下事实：

- free/pro 的精确 cutoff、边界前后各一条、所有租户状态和未知 plan 拒绝；
- 事件无任务、只有终态任务、含 PENDING/SENDING/FAILED 任务时的分别处理；
- 事件删除后任务仍在、`tenant_event_id` 置空、投递日志仍在且不提前级联；
- DONE/CANCELLED/DEAD 的脱钩任务期限，含 DEAD 有日志和无日志两种形态；
- 投递日志 90 天清理不误删未到期记录，不因任务删除提前丢日志；
- claim、retry、SENDING 恢复、complete、failure 与事件清理的两个真实提交顺序；
- 去掉共享锁或状态复查后，对应并发测试必须失败；不能用 `sleep` 或同一连接顺序执行充当并发；
- 多租户同一 `canonical_event_id` 的事件清理互不误删，跨租户共享对象不在本片回收；
- 每批不超过上限，批间等待，清理和 `system_logs` 在同一事务，失败后无半批提交，取消后可重跑；
- 真实 `tenant_cleanup` 连接下的最小列权限、RLS、不能读 payload/snapshot/token、不能改任务状态、只能将终态任务的 `tenant_event_id` 置空、不能删其他表或转授权限；
- 生产路径迁移、幂等重入、连库 prepare/sqlx-check、定向测试、完整门禁、临时调度和构建/Docker 残留清理均有证据；
- 不把本片通过写成 WP-8 通过；共享事件、通知留存和租户注销仍需独立出口判定。

本片实现阶段不读取真实 Telegram/AWS 凭据，不连接生产库，不提交或推送分支；完整门禁和证据记录完成后，再等待另行授权提交与发布。
