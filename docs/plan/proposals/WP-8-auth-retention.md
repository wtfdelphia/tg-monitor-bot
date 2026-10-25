# WP-8：鉴权会话留存清理

任务 id：`WP-8-auth-retention`。

状态：维护人于 2026-10-24 明确宣布 WP-7 通过，并批准按本提案实施 WP-8 首片。
WP-7 的实现和出口复核已随 PR #49 合入，PR 与合并后 CI 均通过。
本批准不授权其他 WP-8 分片；维护人于 2026-10-25 另行授权本批合为一个提交、推送并开 PR 到 `dev`。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-8，
`docs/design/spec/04-数据生命周期与容量.md` §一及 §1.1，
`docs/design/spec/05-安全与租户隔离.md` 的运行时权限纪律，
`docs/design/eng/04-CI门禁.md` 的 RLS 审计与逐项反证。

## 一、现状与复用

基线为 PR #49 合入后的 `dev`：`50c96c1a1054a719b91d25e9c0b0a81baf42620b`。
合并后 push CI run `40055583779` 已通过。提案核查阶段只读源码、迁移与既有测试；
获批实施仅使用本地测试环境，不读取真实凭据或清理生产业务数据。

现有链路已包含部分生命周期能力，不能将 WP-8 记成从零开始，也不能据此宣布完成：

| 已有能力 | 当前边界 |
|---|---|
| `crates/app/src/media_cleanup.rs` | fanout 内执行过期媒体对象、无引用档案和 staging 孤儿清理；结果仅写 tracing，尚未补齐平台 `system_logs` 写入路径 |
| `crates/db/src/repo.rs` 的身份删除 | 删除身份时处理凭据与遗留密钥；不是完整租户注销和宽限期删除 |
| 当前会话注销、远程撤销 | 删除指定会话；没有按留存策略扫描全部过期会话的调度 |
| 扫码挑战终态处理 | 状态过期和成功消费时处理单条记录；不代替后台定期物理清理 |
| `tenant_consents` | 已有凭据代持版本记录和隔离测试；`custody-v0` 是占位版本，不是已批准的对外法律文本 |

复用 SQLx、现有分层错误类型、启动期角色检查、鉴权表及其时间字段，不新增任务表、队列框架或依赖。
租户注销、普通事件清理和法律文本不需要绑成一次实现；先交付可独立验证且不影响活跃业务的过期会话清理。

## 二、分片顺序

| 分片 | 范围与前置条件 |
|---|---|
| 本片：鉴权会话 | 按既有保留规则处理 `console_login_challenges`、`web_sessions`、`bot_console_sessions`；先批准最小权限和清理执行入口 |
| 后续：租户事件与日志 | 处理 `tenant_events`、孤立 DEAD 任务、投递日志、版本、通知去重等；按套餐执行，不能顺便删除同意凭证 |
| 后续：共享事件与媒体 | 保护共享 canonical event 和媒体组引用，串联既有对象清理，补齐平台清理日志；对象失败不得记为清理成功 |
| 后续：租户注销 | 先确定进入 deleted 的可靠时间、停采停投和凭据立即销毁，再做宽限期恢复及物理删除；按 ADR-0029 处理同意凭证 |
| 并行准备、单独批准 | 产品/法务提供稳定版本的同意文本；运维明确举报受理责任、范围与反馈方式，不由实现者编写法律承诺 |

`login_sessions` 属于账号接入域，不与本片三张鉴权表混用角色；其周期清理仍须后续覆盖。
`tenant_notifications` 的 SENT/DEAD 保留期尚待定案，本片不擅自设期限。
上述拆分不是删减 WP-8 出口判据；全部相关规则和租户删除验收完成前，WP-8 不得标为通过。

## 三、请求批准的首片实现

### 3.1 最小权限，而非复用 owner 或扩大业务权限

现有 `auth_lookup` 的 Web 会话读删策略绑定当前 session 或用户，不能当成跨用户过期扫描入口。
`bot_console_sessions` 的全量 GRANT 也不等于有后台删除策略；`platform_ops` 仍只读平台日志。
本片不向 `app_user` 开放鉴权表，不把后台扫描权限加给对外请求使用的 `auth_lookup`。

建议新增独立 `auth_cleanup` 登录角色，只给本片必需的权限：

- 明确 NOSUPERUSER、NOBYPASSRLS、NOCREATEDB、NOCREATEROLE、NOREPLICATION；不属于其他角色，不拥有表、schema 或函数。
- 三张鉴权表只给清理所需列的 SELECT 和 DELETE；不给 INSERT、UPDATE、TRUNCATE、凭据表访问或成员关系写权限。
- `web_sessions`、`bot_console_sessions` 增加仅对该角色生效的 SELECT/DELETE 策略，只允许读取和删除已到期记录；不使用恒真或 PUBLIC 策略，不依赖可任意设置的租户 GUC 放开活跃行。
- `console_login_challenges` 维持既有丙类边界，不启用 RLS。本片角色对该表只获所需列读取和 DELETE，应用层固定到期谓词及批量上限；不声称数据库能限制它只删过期挑战。
- 只新增平台 `system_logs` 的固定清理事件写权限和相应 INSERT 策略，约束 `tenant_id IS NULL`、模块、事件及 context 白名单；不授日志读取、UPDATE、DELETE 或任意平台日志写入。
- 不修改现有角色的权限，不使用常驻 owner 连接、SET ROLE、BYPASSRLS、SECURITY DEFINER 或新数据库扩展。

跨用户清理属于新的凭据隔离边界：执行器只在独立清理进程中拿到该连接串，control-plane、worker 与浏览器不得继承。
角色口令由运行环境提供，迁移不写口令；本地测试凭据与生产分离。

### 3.2 执行入口和删除纪律

建议复用单二进制，新增一次性 `tgm cleanup-auth` 管理命令；由独立定时入口每 10 分钟唤起，Web 会话每小时、
Bot 会话每日的周期由定时入口分别调用固定类别参数实现。调用间隔引用 `spec/04`，不在这里改保留期限。
这是独立凭据边界的管理命令，不是新的长期服务或 HTTP 端点，不将它放进持有其他凭据的 fanout 进程。
新增配置、调度样例及部署文档须一起验证；命令未被实际调度前不能宣称周期清理完成。

本片遵守以下限制：

1. 到期、未到期、恰好边界分别测试；使用数据库时间确定本批 cutoff，删除条件再次核对到期状态。
2. 按完整键有序分批，批量上限与批间等待沿用 `spec/04` §1.1；禁止一次删完整张表。
3. 复用 PostgreSQL 事务级 advisory lock 防同类作业重叠；不为锁额外授予 UPDATE，不持事务锁跨批等待。
4. 每批清理与 `cleanup.done` 同事务提交，记录固定表名、行数、耗时；不包含会话 id、nonce、确认码、用户标识、Token、IP 或 User-Agent。
5. 删除或写日志失败整批回滚，命令非零退出；中断后可重跑。并发续期或 Bot 会话刷新不得被旧候选误删。
6. 使用现有角色检查，并检查实际连接身份确为 `auth_cleanup`；误配为业务、鉴权、owner 或超级用户都拒绝执行。

### 3.3 迁移、审计及变更路径

仅追加幂等迁移，创建角色、列级 GRANT、到期策略、固定平台日志写入策略和清理查询必要的索引；不改已应用迁移、不加表。
`console_login_challenges` 保持丙类，`web_sessions` 和 `bot_console_sessions` 的租户 RLS 与 auth_lookup 自查策略保留。

预计写入范围：

- `migrations/` 的新增文件、`migrations-test/` 的相应测试适配和 `scripts/init/01-roles.sql` 的本地清理角色初始化。
- `crates/db/src/auth_retention.rs`、`crates/app/src/auth_retention.rs` 及模块导出，配套受限角色与执行链路测试。
- `bins/tgm/src/main.rs` 的一次性入口、`crates/core/src/config.rs` 的专用配置校验及相关配置/部署文档。
- `crates/db/src/audit/queries.sql`、`docs/design/eng/04-CI门禁.md`、`scripts/audit-double-pass.sh`：
  增加清理角色权限边界的审计与对应反证；不得只靠自动枚举角色就声称最小权限已被覆盖。
- `docs/design/eng/03-测试策略.md` 和相应规格：同步获批的角色例外、执行入口及其验证边界，不更改保留策略或既有 R 编号含义。

新增断言的 SQL、报红集合与恢复快照必须同轮验证；不跳过已有断言，不扩大其白名单，不修改 CI 判绿条件。
SQL 改动后连库 prepare 并核对 `.sqlx/`，不得用增量编译代替检查。
本片保持既有服务组件和接口边界；若实施发现必须变更架构基线或写新 ADR，应再次升级，而不是把本批准扩成架构授权。

## 四、验收与停止条件

批准后先做最窄受限角色测试，再跑完整本地门禁：

- 真正使用 `auth_cleanup` 登录临时库；不能用管理池代跑清理。管理池仅造种子和核对结果。
- 三张表覆盖到期/未到期边界、跨用户批量、空批次、超批量、多轮重试和并发作业；Web/Bot 活跃行必须保留。
- 验证会话续期与 Bot 刷新竞争，批次中断、日志写失败时回滚，以及失败后安全重跑。
- 验证该角色不能改写活跃会话、创建会话、读取凭据、越权写平台日志、修改成员、拥有表、继承角色或 TRUNCATE。
- `app_user` 仍不能访问鉴权表；现有鉴权、会话撤销、R14/R15/R16、A7 及控制台浏览器路径不退化。
- 调度样例使用独立运行环境和固定参数，验证类别频率、错误退出、重叠退出及停止后不留子进程；未实际验证的生产调度单列为待办。
- 全量迁移可重入、连库 sqlx-check、审计、逐项反证与完整 `scripts/ai-driver/ai-gates.sh` 通过；ignored/skipped 不算通过。

批准前不新增迁移或角色、不运行清理命令、不改安全卡口、不读取生产凭据。
同意文本、租户注销、共享 canonical event、媒体删除和举报入口均不在本片实施授权内。
合并仍由维护人决定；提交、推送和开 PR 另行授权。任务结束清理专属构建缓存及本任务 Docker 残留，保留日志与证据。

提案阶段仅核对 PR #49 和源码；获批后已按上述边界实施。
2026-10-25 完整本地门禁、显式浏览器回归、隔离空库及临时调度验证通过，
结果、失败修正和生产部署待验项见 `../evidence/WP-8-鉴权会话留存-2026-10-24.md`。
未把合并基线 CI 当作本片实现裁决；本批发布已获授权，PR CI 仍待裁决，整个 WP-8 保持未通过。

[report]
task: WP-8-auth-retention
path: ok
gates: pass
escalation: docs/plan/proposals/WP-8-auth-retention.md
