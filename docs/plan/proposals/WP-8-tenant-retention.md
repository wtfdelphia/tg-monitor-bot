# WP-8：租户日志与临时记录留存清理

任务 id：`WP-8-tenant-retention`。

状态：维护人于 2026-10-25 批准按本提案实施，本地实现与完整门禁已通过。
本批准覆盖本片迁移、权限、审计、执行入口、测试和调度样例，不覆盖其他分片。
维护人同日授权本批合为一个提交、推送并开 PR 到 `dev`，随后授权合入 PR #51。
PR CI 与合并后 push CI 均已通过，具体提交与 run 记录见本片证据。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-8，
`docs/design/spec/04-数据生命周期与容量.md` §一与 §1.1，
`docs/design/spec/05-安全与租户隔离.md` §三、§六，
`docs/design/eng/03-测试策略.md` 的受限角色测试纪律及 `docs/design/eng/04-CI门禁.md`。

## 一、基线与复用

基线为 PR #50 合入后的 `dev`：`1ed84aba655cbef65cc1b69d10865b3c5d2bc7a4`。
PR 的 static/database 均通过，合并后 push CI run `40153317080` 的两项检查也通过。
首片已实现鉴权会话清理，但不覆盖本片表；不能以该结果代替本片验收。

只读核查依据为 `migrations/0002_tables_tenant.sql`、`0004_tables_rule.sql`、
`0005_tables_event.sql`、`0006_tables_delivery.sql`、`0007_tables_audit.sql`、
`0010_rls_policies.sql`、`0011_grants.sql` 及当前 repo、投递和清理调用链。

| 已有实现 | 本片复用方式与边界 |
|---|---|
| `crates/db/src/auth_retention.rs`、`crates/app/src/auth_retention.rs` | 复用限批、数据库时间、事务锁、删除与日志同事务、取消时关闭连接的执行方式；不扩大 `auth_cleanup` 的权限 |
| `crates/db/src/tenant.rs` | 沿用先开事务、再绑定租户上下文的纪律；清理取消后不得留下归池等待，不顺便重构所有业务调用方 |
| `crates/db/src/repo.rs` 的租户查询 | 业务 worker 只枚举 active 租户，不足以覆盖留存；清理应按键分页枚举全部状态的租户，不改业务查询语义 |
| `delivery_logs` 的父表隔离 | 日志本身无 `tenant_id`，必须通过 `delivery_tasks` 确认所属租户；不以无租户列为由全表清理 |
| `C8` 与审计双遍 | 复用精确权限白名单和逐项破坏的检查方式，为新角色增加独立断言，不放宽 `C8` |

本片不需要新表、依赖、任务队列或通用清理框架。保留期已经有规格，缺的是受限执行路径及验收。

## 二、本片范围与暂缓项

本片仅处理以下记录，保留期和调用频率引用 `spec/04` §一，不在提案另设期限：

| 类别参数 | 表与删除条件 |
|---|---|
| `login-sessions` | `login_sessions`：`expires_at` 到期，删除时重新检查；不读取或输出 `login_token` |
| `notify-dedup` | `notify_dedup`：`expires_at` 到期，删除时重新检查，完整复合键定位 |
| `bot-console-updates` | `bot_console_updates`：按 `processed_at` 的既定幂等窗口；保留窗口内记录，不能用 `update_id` 大小代替时间 |
| `delivery-logs` | `delivery_logs`：按 `created_at` 到期，通过父任务约束租户；不删父任务 |
| `audit-logs` | `audit_logs`：仅非空租户行，按该租户当前 plan 和 `created_at` 执行既定分档规则 |
| `system-logs` | `system_logs`：仅非空租户行，按 `created_at` 到期；不删平台级日志 |

这些表在当前迁移中没有入向外键引用，清理不触发本仓其他业务表的级联删除。
新增迁移若改变该依赖形状，必须重新核对本边界，不能继续照旧宣称无级联影响。

本轮不把以下工作混入：

- `tenant_events`：删除会级联删掉投递任务及日志。当前 delivery 领取任务后提交事务，再发外部请求，
  清理不能只看父事件年龄就把已领取或待处理任务删掉；在途保护与到期停投语义需单独批准。
- 孤立 DEAD `delivery_tasks`：已有人工重试会将 DEAD/FAILED 改回 PENDING，清理须和重试竞争一起验证。
  终态年龄的计时字段也需明确，本片不擅自选择 `created_at` 或终态时间。
- `rule_versions`：即使年龄到期，也须核对当前版本与保留中投递记录的快照反查。
  本片不删版本，不把 `audit_logs` 已清理等同于版本留存已完成。
- `canonical_events`、媒体组、媒体档案和对象存储：后续单片保护共享引用、补齐平台清理日志。
- `tenant_notifications`：终态保留期尚未定案；`tenant_consents` 不主动清理，仍按 ADR-0029 随租户物理删除。
- 租户注销、凭据销毁、法律文本、举报入口及第三方删除请求均不在本片范围。

本片故意先交付无级联删除的部分，不减少 WP-8 的总出口判据。其他规则缺失时仍须明确记未完成。

## 三、请求批准的实现

### 3.1 独立受限角色与清理日志

新增直接登录的 `tenant_cleanup`，不复用对外请求的 `app_user`、鉴权角色或 owner：

- 无超级用户、BYPASSRLS、建库、建角色、复制、成员关系、对象所有权和用户 schema CREATE 权限。
  生产迁移不写口令，本地初始化独立预置测试身份。
- 三类临时表及三类日志只给必要键、租户键、时间列的 SELECT 和 DELETE。
  `tenants` 仅给 `id`、`plan` 的列级读取，`delivery_tasks` 仅给 `id`、`tenant_id` 的列级读取。
  不授任务状态更新、凭据、登录 token、快照、正文、IP 或日志详情读取权限。
- 租户表策略同时要求当前事务租户匹配和记录到期；`delivery_logs` 经当前租户的父任务关联。
  读取父任务只用于隔离，不授 DELETE；审计日志的期限按租户 plan 判断。
  策略限新角色，不加 PUBLIC/恒真策略，不改变现有角色或 FORCE 边界。
- 允许以当前租户写固定 `tenant_retention/cleanup.done` 日志；INSERT 只开放必要列，
  约束级别、模块、事件和 context 白名单，context 仅含表名、删除行数、耗时。
  不授日志 UPDATE，不允许写 `tenant_id IS NULL`、回填旧时间或携带业务标识；仅授对应序列 USAGE。
- 生成的清理日志由数据库默认时间写入，不会在同批到期查询中删除。日志仍受既定留存规则约束，
  本片不借清理名义补开平台日志写权限。

租户 GUC 是受信清理进程选择租户的上下文，不是不可伪造凭证；不得声称它能约束该进程永远只清一个租户。
本角色本来就需轮流处理全部租户，但数据库仍须限制到期行和动作，不能伪造 GUC 后删除未到期行。
测试必须使用新角色真实连接，不能以有测试恒真策略的管理池代跑清理。

### 3.2 执行入口、竞争与停止

建议新增一次性 `tgm cleanup-tenant --category <固定类别>`，仅接收
`TGM_PROFILE`、`TGM_DB_URL_TENANT_CLEANUP` 及可选 `RUST_LOG`。
拒绝混入其他已登记凭据和 `DATABASE_URL`；常驻服务反向拒绝持有专用连接串。
不创建 KMS、Telegram、Redis 或对象存储客户端，不在 HTTP/Bot 入口接受租户列表或清理条件。

1. 按租户主键分页枚举全部状态，不一次加载全部租户，不用 active-only 查询漏掉 suspended/deleted。
2. 每批开事务后设置租户上下文，固定类别和完整键有序限批；cutoff 取数据库时间。
   删除时重新核对当前时间字段，保证并发续期/刷新不会被旧候选误删。
3. 审计日志以删除语句可见的已提交 plan 判定；不跨批缓存套餐或发现未知值时默认按 free 删除。
   批次开始后的套餐升级不追溯恢复已删记录；变更与清理的先后语义及快照边界须用并发测试写清。
4. 同租户、同类别用事务级 advisory lock 互斥；重叠正常退出该批，不写清理成功日志。
   批间释放事务和锁，批量及等待沿用 `spec/04` §1.1，不持锁跨等待。
5. 删除与固定租户清理日志同事务提交；任一 SQL 或日志写入失败整批回滚，命令非零退出。
   SIGTERM 停止未提交批次，保留已提交批次，取消连接按首片教训处理，不能靠等行锁释放才退出。
6. 提供独立系统用户、环境文件和固定类别的 systemd 样例，频率与规格一致；
   本地临时调度和生产身份、真实周期、告警分别验收，不因样例存在便称已启用。

### 3.3 迁移、审计与写入范围

仅追加幂等迁移，创建角色、列级授权、限定角色的策略和实际查询所需索引，不改旧迁移。
为 `tenant_cleanup` 新增独立 `C9` 权限白名单，核对必要列漏授、多授、成员、对象所有权、
MAINTAIN 与转授权限；不放宽既有断言，不改 CI 判绿条件。
同步 `eng/04` 与执行 SQL，扩展双遍破坏、撤销及目录快照检查。

实施时双遍发现：新增清理策略会掩盖 `C1` 的业务策略缺失。保留原破坏方式，
将 `C1` 收紧为检查业务角色可用的策略并补回归；不放宽既有隔离判据。

预计写入：`migrations/`、`migrations-test/` 的新增迁移，`scripts/init/01-roles.sql`，
`crates/db/src/tenant_retention.rs`、`crates/app/src/tenant_retention.rs` 及导出，
`bins/tgm/src/main.rs`、`crates/core/src/config.rs`、对应测试，
`crates/db/src/audit/`、`scripts/audit-double-pass.sh`、`deploy/systemd/`，
以及配置、权限、留存语义、测试、部署、里程碑和本片证据文档。
SQL 改后连库 prepare；若无需新增宏查询，`.sqlx/` 无变化也须实际核对。

不为本片创建任务表或调度框架；只有明确共用且有检查的逻辑才提取，不能趁机重构鉴权首片。
若实施需要新增 ADR、改变基线保留期或扩大到被暂缓表，重新升级。

## 四、验收与交付边界

- 每类覆盖到期、未到期、精确边界、空批次、超批量与重跑；相同业务键在不同租户中保持独立。
- 非 active 租户也按规则清理；free/pro 审计日志分档、批间套餐变更与并发提交顺序可重复验证。
- `delivery_logs` 必须经父任务判租户；平台级日志、未到期记录、事件、任务、版本、凭据及同意凭证保留。
- 续期/刷新、同类重叠、日志失败及锁等待中 SIGTERM 后回滚与重跑均需真实并发测试，不以 sleep 猜时序。
- 以受限角色证明不能读取敏感列、改写任务或会话、删除无授权表、写任意/平台日志、继承角色或 TRUNCATE。
- 新断言及逐项破坏报红集合可核对，撤销后目录快照一致；原 `C8` 与其他隔离断言不退化。
- 生产迁移、迁移重入、连库 prepare/sqlx-check、完整本地门禁通过；ignored/skipped 不计通过。
  对受影响的会话、控制 Bot、投递日志与浏览器路径补回归，真实外部凭据不用于本片本地验收。
- 实际验证临时调度触发、独立环境、失败非零、停止无子进程和中断后重跑；生产部署未验项单列。
  结束检查磁盘并清理专属构建缓存及临时 Docker，保留源码、`.sqlx/`、失败日志与证据，不删业务卷。

只读提案阶段没有运行完整门禁或数据库清理；获批后只使用本地测试环境实施。
新迁移、安全面与审计在本片批准范围内，不读取真实 Telegram/AWS 凭据，不清理生产数据。
最终版本已从头跑完完整本地门禁；定向反证、浏览器回归、隔离空库与临时调度结果，
以及首次双遍失败和生产部署待验边界见 `../evidence/WP-8-租户日志与临时记录留存-2026-10-25.md`。
本片已合入 `dev` 并通过 CI；该结果不代表 WP-8 通过或生产上线批准，生产部署待验项不变。

[report]
task: WP-8-tenant-retention
path: ok
gates: pass
escalation: docs/plan/proposals/WP-8-tenant-retention.md
