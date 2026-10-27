# WP-8：平台系统日志到期清理实施证据

维护人于 2026-10-27 批准 `../proposals/WP-8-platform-log-retention.md`。
本片在隔离工作树
`/home/openclaw/wtf_workspace/local/tg-monitor-bot-release/wp8-platform-log-retention`
实施，基线为 PR #57 合并提交 `97c449bbb2d0ce93642f47310a51df017434ce31`。
本记录对应提交前的本地验收；维护人同日授权本批合为一个提交、推送并开 PR 到 `dev`。
未读取生产凭据，未连接生产数据库、Telegram 或 AWS。

判据出处：`../../design/plan/00-里程碑与工期.md` §4.3 WP-8，
`../../design/spec/04-数据生命周期与容量.md` §一、§1.1，
`../../design/spec/05-安全与租户隔离.md` §六，
`../../design/eng/03-测试策略.md` §二、§三及
`../../design/eng/04-CI门禁.md` 的 C13 与审计双遍要求。

## 一、实现范围

- 追加正式迁移 `0037_platform_retention.sql` 及对应测试迁移，未修改旧迁移。
  新角色 `platform_cleanup` 只读候选键和时间列、删除到期平台日志，不能读取 context 或业务表。
  SELECT/DELETE 策略同时限制 NULL 租户和到期条件；应用查询另行显式筛选。
- 新增 `tgm cleanup-platform`、独立连接串 `TGM_DB_URL_PLATFORM_CLEANUP` 和配置隔离检查。
  启动核对直接登录身份、角色属性及成员关系；拒绝其他已登记凭据与 `DATABASE_URL` 混入。
  普通服务和其他清理命令同样拒绝持有本片连接串。
- 按数据库 cutoff、完整 `(created_at, id)` 顺序限批，删除时再次核对资格。
  类别事务锁互斥，批间等待；仅接受 READ COMMITTED。
  每批删除与固定 `platform_retention/cleanup.done` 日志同事务，失败回滚。
  取消时关闭连接；CLI 对重叠、中断和失败非零退出，错误保持脱敏。
- 新增 C13 精确权限审计及双遍反证，文档 SQL 与实现逐字一致。
  增加独立 systemd service、daily timer、环境样例，并同步配置、生命周期和部署文档。

## 二、专项与反向验证

本地 Compose PostgreSQL 上的专项命令：

```bash
DATABASE_URL=postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control \
SQLX_OFFLINE=true CARGO_TARGET_DIR=target/ai-gates-task \
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
cargo test --workspace --test platform_retention
```

首轮专项中，CLI、应用和数据库已执行用例全部通过；随后补入直接 DELETE 策略哨兵。
最终完整门禁中，本片 CLI 为 2 passed，应用为 1 passed，数据库集成为 7 passed，
均无失败或跳过。数据库模块的精确 cutoff 测试同样通过；该专项筛选还执行了既有 cutoff 测试，
不把其全部用例算作本片新增。

覆盖范围：

- 只删到期平台行，保留近期平台日志和 active/suspended/deleted 租户日志；历史清理日志同样到期回收。
  完成日志计数与实际删除一致，零行重跑也写固定日志。
- 受限连接不能读取 context、module 或业务表，不能 UPDATE/TRUNCATE；设置租户上下文也不能越界。
  伪造租户、日志类型、module/event、context、时间或 id 均被拒绝；其他写入角色不能伪造本片日志。
- 超单批候选、重叠、日志失败回滚、重复执行、精确 cutoff 与下一微秒保护。
  并发插入留待下一批，真实行锁等待后刷新时间的行在删除阶段被复查保留。
- 应用分批与等待、预取消、真实行锁等待中取消并释放连接、重跑恢复；CLI SIGTERM 非零退出且未提交删除回滚。
  不支持的隔离级别直接拒绝；C13 对漏授、多授、列权限及序列权限转授报红，恢复后该项回绿。

## 三、隔离空库与调度验收

任务专属 PostgreSQL 容器使用 `--rm --network none`、tmpfs 数据和 Unix socket，
挂载当前初始化脚本，使用本片二进制执行 `tgm migrate`、`audit-rls` 和真实清理。
空库迁移及审计通过，C13 在位；到期平台行删除，近期平台行和 deleted 租户日志保留，日志计数一致。

实际策略反证：

- 放宽 SELECT/DELETE 的到期限制，近期平台行保护哨兵按预期报红。
- 移除 NULL 租户限制，租户日志保护哨兵按预期报红。
- 反证删除在事务中回滚，每轮恢复原策略后哨兵重新通过，最后整库审计再次通过。

临时 user timer 实际触发 `tgm cleanup-platform`，结果为
`MainPID=0 / Result=success / ExecMainStatus=0`；新增到期行删除，保护行仍在。
service 样例替换为本地二进制路径后，`systemd-analyze verify` 退出 0，daily 日历解析通过。
主机既有 netplan 权限和 snapd `RestartMode` 警告未导致检查失败。
临时 user timer 不等于生产系统用户、Secret 权限、日周期或告警已验收。

## 四、完整门禁与失败修复

修复后从头执行完整门禁，不使用 `--from` 跳过前置卡口：

```bash
DATABASE_URL=postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control \
TGM_DB_URL_OWNER=postgres://postgres:pw@127.0.0.1:55432/tgm \
TGM_DB_URL_TASK_SCHEMA=postgres://app_user:apw@127.0.0.1:55432/tgm_test_schema \
TGM_DB_URL_TASK_SCHEMA_OWNER=postgres://postgres:pw@127.0.0.1:55432/tgm_test_schema \
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
scripts/ai-driver/ai-gates.sh
```

最终进程退出 0，日志包含“已执行的全部本地卡口通过”：

- fmt、clippy、offline-build、deny、newtype、OpenAPI 契约、驱动回归通过。
- 生产路径迁移、在线 SQLx check、迁移幂等通过；正式迁移强制重跑后目录快照逐字节一致。
- 工作区测试汇总为 461 passed / 0 failed / 21 ignored。需要 Telegram、浏览器或外部环境的用例
  仍保持 ignored，未把跳过计为验收通过；本片不涉及浏览器或 Telegram 变更。
- `audit-rls` 全绿，包含 C13；审计双遍逐条破坏命中预期，额外授予 context 读取权限时仅 C13 报红。
  收尾目录快照与第一遍逐字节一致。
- 门禁结束后另以 `app_user` 连接 schema 库执行 `cargo sqlx prepare --workspace`，退出 0，
  再执行在线 `--check` 退出 0，无 unused queries 警告。
  `.sqlx/` 无 Git 差异；未修改依赖、OpenAPI 契约或旧迁移。

本地日志保存在 `ai-driver-logs/WP-8-platform-log-retention/2026-10-27/`，该目录被 Git 忽略：

- `focused.log`、`cutoff.log`：专项测试。
- `acceptance-final.log`：隔离空库、策略反证和临时 timer 最终成功记录。
- `units.log`：systemd 静态检查与日历解析。
- `full-gates-final.log`：修复后从头执行的完整门禁。
- `sqlx-prepare.log`、`sqlx-check-final.log`：独立在线 prepare 与复查记录。
- `resource-cleanup.json`：专属构建缓存清理与磁盘回收记录。

初轮失败与处理：

- 新配置单测漏导入校验函数，首轮 clippy 失败；补齐导入后从头重跑完整门禁。
- 测试曾要求 SQLx 临时库整体审计全绿，但该库含专用恒真策略及测试角色拥有的迁移表。
  改为核对本片 C13 恢复，正式迁移库仍必须通过完整审计，未放宽生产判据。
- 临时 timer 验收脚本把 Unix socket URL 的 `%2F` 直接写入 `Environment=`，被 systemd 当作 specifier。
  修正验收脚本转义后，重新执行整个空库、反证和 timer 验收，通过；部署样例未使用该 URL。

首次失败日志均保留。本地 `cargo-deny` 为 `0.20.2`、CI 固定 `0.19.1`，
依赖警告与版本漂移如实记录，未修改 CI 判据。

## 五、资源清理与交付边界

完整门禁及在线 SQLx 复查结束后，检查专属缓存的进程工作目录、可执行文件、文件描述符和内存映射引用，
未发现本片缓存仍被使用。本片 `target/ai-gates-task` 与 `target/ai-gates-trusted` 已清理，
按目录占用回收约 5.1 GiB，根盘可用空间从约 35 GiB 增至 40 GiB。
临时编译日志从 `/tmp/tgm-platform-compile.log` 归档到本任务日志后删除原文件；
源码、`.sqlx/`、失败日志及证据均保留。

隔离验收容器、socket 和临时 systemd 单元已清理。收尾仅有已有 `kms-kek` Compose 服务，
未发现悬空卷或本片新增网络；未执行数据库销毁或全局 Docker prune。
`lsof` 对主机 tracefs、Docker overlay/netns 给出权限警告，未发现专属缓存引用；
清理前另用 `/proc` 核对本片进程引用，不以 Docker 文件系统不可读作为清理既有资源的理由。

本轮 ponytail-review 已核对调用链、权限边界与实现 diff：复用既有清理配置及预检，
未引入通用框架、新依赖或无要求的抽象，也未增加需登记的 `ponytail:` 简化。
`git diff --check` 通过；PR #57 的发布台账与媒体日志债务升级记录随本片补齐。

本地验收不代表 PR CI、生产迁移、Secret 注入或周期清理已经通过。
本批已获提交、推送和开 PR 授权，合入仍须 PR CI 裁决与维护人授权；不宣布 WP-8、G3、G4 或生产部署通过。
租户注销、同意文本和举报入口仍按独立范围推进。
