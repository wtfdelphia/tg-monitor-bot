# WP-5-delivery-media 片 3 B：媒体对象清理（2026-10-20）

提案：`docs/plan/proposals/WP-5-delivery-media.md`（片 3 于 2026-10-20 批准，批准记录第 6、7 条）。
本文记录 L2、反证与完整门禁的结果。A（媒体组聚合）另开 PR，不在本文范围内。

## 一、实现

- `crates/app/src/media_cleanup.rs`：`run_cleanup_once` 依次做三步，每一步都幂等：
  1. `object_expires_at` 已过的档案：先删对象，删成功再把 `object_key` / `object_expires_at` 置空；
  2. 超过 180 天、已无对象、没有 `media_relay_jobs` 引用的档案：物理删除；
  3. `staging/` 前缀下超过 1 天的对象：删除。
  前两步每批不超过 5000 行，批间 200 ms。
- `crates/db/src/repo.rs`：`list_expired_media_objects`、`clear_media_archive_object`、
  `delete_stale_media_archive`。置空条件带上旧 key 与过期判定，清理期间中转换上的新 key 不会被覆盖。
- `crates/app/src/pipeline.rs`：`FanoutWorker::with_media_cleanup`，启动后先跑一轮，之后每小时一轮；
  失败只记 warn，不影响扇出。`bins/tgm/src/main.rs` 的 `serve_fanout` 接上对象存储。
- `crates/core/src/config.rs`：fanout 必填 `TGM_S3_ENDPOINT` / `TGM_S3_BUCKET` /
  `TGM_S3_ACCESS_KEY_ID` / `TGM_S3_SECRET_ACCESS_KEY`，可选 `TGM_S3_REGION`；
  fanout 仍不接受 AWS 凭据与 KEK。

没有新增迁移和依赖。新查询是运行时 query，`.sqlx/` 不变。

## 二、L1

`s3_credentials_are_media_roles_only` 改为：mtproto、delivery、fanout 三个角色必填 S3 静态凭据，
rule 与 control-plane 配了即报多配。

## 三、L2（compose 的 PG 与 MinIO，以 `app_user` 连接）

`crates/app/tests/media_cleanup.rs`，5 条全绿：

| 用例 | 判据 |
|---|---|
| `expired_objects_are_deleted_then_cleared` | 过期对象被删、key 置空；未过期的对象与 key 不动；第二轮没有可清的 |
| `clearing_does_not_overwrite_a_replaced_key` | 列出之后中转换了新 key，置空不生效，新 key 保留 |
| `stale_archives_are_deleted_unless_referenced` | 181 天、无对象、无 job 的删掉；179 天的、被 job 引用的、仍挂着对象的不删 |
| `staging_orphans_are_deleted_by_age` | 超龄的 `staging/` 对象删掉；新写的不动；`staging/` 之外的不动 |
| `failed_object_delete_keeps_the_archive` | 对象存储不可用时 key 不置空，整轮返回错误 |

`bins/tgm/tests/worker_restart.rs` 里的 fanout 子进程改为带 MinIO 配置启动（桶 `tgm-fanout`）。

## 四、反证

逐条破坏实现、只跑对应用例，7 条全部变红，源码随后还原：

| 破坏 | 变红的用例 |
|---|---|
| 删对象失败时照样置空 | `failed_object_delete_keeps_the_archive` |
| 置空去掉 key 与过期条件 | `clearing_does_not_overwrite_a_replaced_key` |
| 档案删除去掉 `NOT EXISTS` job 条件 | `stale_archives_are_deleted_unless_referenced` |
| 档案删除去掉 `object_key IS NULL` | `stale_archives_are_deleted_unless_referenced` |
| 档案删除去掉 180 天保留期 | `stale_archives_are_deleted_unless_referenced` |
| staging 去掉年龄过滤 | `staging_orphans_are_deleted_by_age` |
| 未过期对象也列出 | `expired_objects_are_deleted_then_cleared` |

「先置空再删对象」这种顺序错误由第一条覆盖：置空发生在删除结果之前，删除失败时 key 已经丢了。

## 五、完整门禁

`scripts/ai-driver/ai-gates.sh`（带库，compose 的 PG / PgBouncer / Redis / MinIO）rc=0，
输出「已执行的全部本地卡口通过」与「double-pass 通过」。测试合计 326 passed / 0 failed / 11 ignored，
比上次基线（321）多出的 5 条就是本片的 `media_cleanup` 用例；`worker_restart` 带 MinIO 配置启动 fanout 后仍然全绿。

## 六、未覆盖项

- spec/04 §1.1 第 2 条要求写 `system_logs`，本片只写 tracing（`event="cleanup.done"`）。
  `app_user` 写不进 `tenant_id IS NULL` 的行，平台级日志的写入路径需要另定，位置见
  `media_cleanup.rs` 的 `ponytail:` 注释。
- 只在 MinIO 上验证过，生产 S3 的 `ListObjectsV2` / `DeleteObject` 权限与分页尚未实测。
- 多个 fanout 实例同时清理只靠各步幂等，没有并发 L2。
