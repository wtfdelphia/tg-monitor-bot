# WP-8：媒体清理平台日志闭环实施证据

维护人已批准 `../proposals/WP-8-media-cleanup-logs.md`。本记录来自隔离工作树
`/home/openclaw/wtf_workspace/local/tg-monitor-bot-release/wp8-media-cleanup-logs`，
基线为 `cloud/dev`，当前分支为 `codex/WP-8-media-cleanup-logs`。
本片未连接生产数据库、生产 Telegram 或生产 AWS，也未读取或提交真实凭据。

判据出处：`../../design/plan/00-里程碑与工期.md` §4.3 WP-8，
`../../design/spec/04-数据生命周期与容量.md` §一、§1.1，
`../../design/spec/05-安全与租户隔离.md` §六，
`../../design/eng/03-测试策略.md` 与 `../../design/eng/04-CI门禁.md` 的 C12 和审计双遍要求。

## 一、实现范围

- 新增 `media_cleanup` 受限数据库角色及正式/测试迁移，最小化授予 `media_archive`、
  `media_relay_jobs.media_archive_id` 和固定平台日志写入所需权限。
- 新增 `system_logs` 的平台级 `media_retention/cleanup.done` 写入策略，并加入 `C12` 审计
  和双遍反证；日志 context 仅允许 `table`、`rows`、`elapsed_ms`，表名和行数受约束。
- 新增 `TGM_DB_URL_MEDIA_CLEANUP`，fanout 启动时建立并校验独立连接池；缺失、错误角色
  或不安全角色属性均在启动/清理前失败，不回退到 `app_user` 或 `shared_cleanup`。
- 过期对象删除成功后，key 清空与平台日志在同一数据库事务；档案物理删除与日志同一事务；
  `staging/` 成功删除后写固定日志。对象删除失败保留档案 key，日志失败回滚数据库变更。
- 补充媒体清理专项测试、fanout 重启测试配置，以及配置、迁移、审计和生命周期文档。

## 二、专项验证

在本地 Compose PostgreSQL 与 MinIO 环境中，以受限角色执行媒体清理测试：

- `crates/app/tests/media_cleanup.rs`：6 passed、0 failed。
- 覆盖过期对象删除和 key 清空、未过期对象保护、staging 孤儿清理、媒体档案引用保护、
  替换 key 的并发保护、对象删除失败保留 key、重复执行幂等和日志失败回滚。
- `bins/tgm/tests/worker_restart.rs`：3 passed、0 failed；fanout 子进程注入
  `TGM_DB_URL_MEDIA_CLEANUP`，重启恢复路径通过。
- `git diff --check`：通过。

媒体测试明确核对数据库身份为 `media_cleanup`；日志断言核对三阶段固定表名、成功行数和
零行阶段记录。日志失败用例撤销媒体日志 INSERT 权限后确认：对象已按既有外部删除顺序处理，
数据库 key 更新回滚，未产生伪造的 `cleanup.done` 成功日志。

## 三、完整门禁

静态及迁移前置卡口首轮通过：fmt、clippy、offline-build、deny、newtype、OpenAPI、驱动回归、
生产路径迁移、在线 SQLx check 和迁移幂等。

随后执行：

```bash
DATABASE_URL='postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control' \
TGM_DB_URL_OWNER='postgres://postgres:pw@127.0.0.1:55432/tgm' \
TGM_DB_URL_TASK_SCHEMA='postgres://app_user:apw@127.0.0.1:55432/tgm_test_schema' \
TGM_DB_URL_TASK_SCHEMA_OWNER='postgres://postgres:pw@127.0.0.1:55432/tgm_test_schema' \
./scripts/ai-driver/ai-gates.sh --from test
```

结果：

- 工作区测试阶段退出 0，执行用例均通过；明确需要 Telegram、真实浏览器或外部凭据的用例
  仍按定义保持 ignored，未被计作真机通过。
- `audit-rls`：23 条断言、0 条报红，包含新增 `C12`。
- `audit-double-pass.sh`：未破坏库通过；逐条破坏均命中预期断言；收尾目录快照 868 行
  与第一遍逐字节一致。
- 门禁最终退出 0；未使用生产凭据，未连接生产环境。

本地 `cargo-deny` 为 `0.20.2`，CI 固定 `0.19.1`；已有重复依赖版本警告未扩大本片范围，
未修改 CI 规则。

## 四、资源清理与边界

- 完整门禁结束后确认没有 `ai-gates.sh`、Cargo 测试或构建进程继续引用本片专属缓存。
- 本片专属 `target/ai-gates-task` 与 `target/ai-gates-trusted` 已在确认无进程引用后安全清理，
  共回收约 29.9 GiB；源码、`.sqlx/`、测试证据和失败日志保留。
- `docker ps -a` 仅发现已有 `kms-kek` Compose 栈；无悬空卷、无本片新增网络，未执行
  `docker compose down -v` 或全局 Docker prune。
- 本片不宣布整个 WP-8 通过；租户删除、同意文本、举报入口及生产部署仍按独立范围验收。

## 五、发布状态

本片已完成实现和本地验收，但当前仍未提交、未推送、未开 PR。后续发布需维护人另行明确授权，
并按“一次提交、推送到 `dev`、开 PR、等待 CI”执行；PR CI 才是最终裁决。

[report]
task: WP-8-media-cleanup-logs
path: ok
gates: pass
escalation: docs/plan/proposals/WP-8-media-cleanup-logs.md
