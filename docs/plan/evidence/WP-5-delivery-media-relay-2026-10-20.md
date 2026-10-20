# WP-5-delivery-media 片 2：跨身份媒体中转（2026-10-20）

提案：`docs/plan/proposals/WP-5-delivery-media.md`（片 2 于 2026-10-20 批准，批准记录 1–6）。
本文记录 L1、L2、反证、真机验收和完整门禁的结果。

## 一、L1

`cargo test -p tgm-app --lib`：66 passed；`cargo test -p tgm-tg --lib`：34 passed。片 2 新增的断言覆盖：

- `caption_plan` 的 1024 / 4096 边界；带 emoji 的说明按 UTF-16 码元计数；
- `split_text_random_id` 恒为正，且与原值不同；
- 新增永久失败码 `MEDIA_CAPTION_TOO_LONG`、`MESSAGE_TOO_LONG` 的分类；
- `media_route` 的表驱动用例（Direct / TextOnly / Relay / TooLarge）；
- `resolve_peer_ref_prefers_cached_access_hash`：缓存命中时使用缓存的 `access_hash`（见第四节）；
- `s3_credentials_are_media_roles_only`（`crates/core/src/config.rs`）：只有 mtproto 和 delivery 角色必填 S3 静态凭据。

## 二、L2（compose 的 PG 与 MinIO，以 `app_user` 连接）

`crates/app/tests/media_relay.rs`，7 条全绿：

| 用例 | 判据 |
|---|---|
| `only_pro_media_creates_one_shared_job` | pro 档建 job，free 档不建；两个 pro 租户命中同一事件只建一个 job；free 载荷标记 `text_only` |
| `free_media_without_caption_uses_placeholder` | free 档无说明的媒体，正文为 `[媒体：DOCUMENT] <来源链接>` |
| `oversized_media_is_dead_at_enqueue` | 超过 50 MiB 时入队即 `DEAD MEDIA_TOO_LARGE`，不建 job |
| `relay_downloads_archives_dedups_and_unblocks_delivery` | 未就绪时推后且 `attempt_count` 仍为 0；下载进 MinIO 逐字节一致；同 hash 第二个事件复用旧对象；就绪后投递带上对象与元数据 |
| `relay_failures_back_off_then_die_and_delivery_follows` | 30s / 5min / 30min 退避，第 4 次失败判死，delivery 沿用 job 的错误码；源消息被删（`SOURCE_MESSAGE_MISSING`）一次判死 |
| `delivery_times_out_and_stale_lease_cannot_claim` | 任务超过 30 分钟判 `MEDIA_RELAY_TIMEOUT`；租约不在手时领不到 job |
| `relay_state_is_not_readable_across_tenants` | 别的租户的任务 id 读不到中转状态 |

`media_relay_jobs` 是丙类表，`tgm_test` 注入池对它没有权限，测试造数也用 `app_user` 池。

## 三、反证

逐一破坏 9 处实现，对应的 L2 用例都变红。其中「同 hash 复用」一条是通过破坏 `media_archive`
upsert 的 `CASE` 让它变红的。实施过程中曾写过 `find_live_media_archive` 和 `mark_media_relay_ready`，
反证表明它们是冗余路径，已经删除。

跨租户这一条要单独说明：只破坏 `load_media_relay_for_task` 里的 `WHERE tenant_id` 时，
测试**仍然是绿的**。原因是查询在 `tenant_tx` 下执行，`delivery_tasks` 与 `tenant_events`
的 RLS 已经把别的租户的行拦掉了。这条判据实际由 RLS 守住，应用层的 `WHERE` 是第二层，
单独破坏它测试不会变红。

审计断言 C7（丙类表必须存在且 RLS 关闭）在 `scripts/audit-double-pass.sh` 中的破坏方式是
`ENABLE ROW LEVEL SECURITY`，恢复方式是 `DISABLE`。

## 四、真机验收

工具：`crates/app/tests/media_relay_live.rs`（`#[ignore]`）。
采集身份是扫码登录的测试用户账号（home DC 5），session 由测试工具 `crates/tg/tests/qr_login.rs`
写出，存放在仓库外，权限 0600。发送身份是测试 Bot 1。源频道和目标频道都是 `@tg_monitor_demo`。
凭据只通过 `~/.config/tgm-live.env` 传入，输出不含 Token、`api_hash` 和 session。

```bash
set -a; . ~/.config/tgm-live.env; set +a
TGM_LIVE_TARGET_USERNAME=tg_monitor_demo TGM_LIVE_USER_SESSION_FILE=~/.config/tgm-qr/user.session \
  SQLX_OFFLINE=true cargo test -p tgm-tg --test qr_login -- --ignored live_user_session_resume --nocapture
DATABASE_URL=postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control \
TGM_LIVE_TARGET_USERNAME=tg_monitor_demo TGM_LIVE_USER_SESSION_FILE=~/.config/tgm-qr/user.session \
  SQLX_OFFLINE=true cargo test -p tgm-app --test media_relay_live -- --ignored --nocapture
```

原始输出：

```text
[resume] ambient_read=err request error: rpc error 400: CHANNEL_INVALID caused by channels.getMessages
[resume] is_bot=false home_dc=5 recent_messages=3
[R-short] source_message_id=189 caption_utf16=32 relay=ready took=2.673692189s byte_equal=true delivered_message_id=190 send_took=2.963314561s same_random_id_resend_message_id=190
[R-split] source_message_id=191 caption_utf16=1123 relay=ready took=2.980670491s byte_equal=true delivered_message_id=192 send_took=3.136835723s same_random_id_resend_message_id=192
[R-probe] chars=1024 utf16=1027 result=ok message_id=194
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 45.40s
```

每个用例的流程：Bot 1 发一个 1 MiB 的随机源文件，测试落一条 canonical event 并跑规则；
第一次投递被推后；用户账号经 `MediaReader` 重新取消息并分片下载进 MinIO，内容逐字节一致；
第二次投递由 Bot 1 上传并发出；最后用同一 `random_id` 再投一次，返回同一个 `message_id`。

| 项 | 结果 |
|---|---|
| 用户账号作为下载方 | 成功，1 MiB 约 2.7–3.0 s |
| 短说明一条发完 | 消息 190 |
| 超过 1024 拆两条 | 媒体（消息 192，不带说明）后跟一条全文（消息 193，使用派生 `random_id`） |
| `random_id` 重发去重 | 两个用例都返回原消息 id（190、192） |
| 说明长度口径 | 1024 个字符、1027 个 UTF-16 码元的说明被服务器接受：服务器不是按 UTF-16 计的。本仓按 UTF-16 判定是偏保守的做法，只会多拆，不会被服务器拒绝 |

**真机发现的缺陷（已修）**：用户账号用 ambient 引用（`access_hash` 为 0）读频道，
服务器返回 `400 CHANNEL_INVALID`（见 `[resume]` 第一行，片 1 spike 的 M5 在 Bot 上是同样结果）。
原来的 `resolve_peer_ref` 只走 ambient 解析，生产中的采集身份下载会全部失败。
现在改为先查 session 缓存里的 `access_hash`，缓存里没有才回落到 ambient
（`crates/tg/src/client.rs` 的 `resolve_peer_ref`）。真机这一轮走的是缓存路径。
L1 `resolve_peer_ref_prefers_cached_access_hash`（`crates/tg/src/client.rs`）在从未连网的客户端上
验证缓存命中时直接返回缓存里的 `access_hash`。反证：去掉缓存分支后，这条测试报 `Invocation(Dropped)` 变红。
缓存由恢复工具解析频道用户名时写入；生产中的采集身份在收到频道更新时，grammers 会把频道写进同一缓存。
后一条本轮**没有**在真机上单独验证。

本轮在测试频道留下消息 189–194，可以人工删除。消息 193 是拆分出的全文，
`delivery_tasks` 不记录它的 id（批准记录 2）。

## 五、完整门禁

`scripts/ai-driver/ai-gates.sh` 全部本地卡口通过：fmt、clippy、offline-build、deny、newtype、openapi-check、
driver-regressions、migrate、sqlx-check、migrate-idempotent（21 个迁移文件重跑后库状态逐字节相同）、
test（321 passed、0 failed、11 ignored）、audit-rls（含 C7）、double-pass（单独破坏 C7 时恰好只有它一条报红）。
最终裁决以 CI 为准。

## 六、未覆盖

- QR 登录走的是测试工具（`crates/tg/tests/qr_login.rs`），**不宣称 WP-2 的 QR 出口判据达成**。
- `file_reference` 过期后重取：实现上每次下载都重新取消息，没有构造过期场景验证。
- 生产 mtproto-worker 从更新流缓存频道 `access_hash`（见第四节）。
- 媒体组、对象清理（片 3）；视频和音频的 document attributes（代码中有 `ponytail:` 注释）；
  按 document id 在下载前跳过（代码中有 `ponytail:` 注释）。
