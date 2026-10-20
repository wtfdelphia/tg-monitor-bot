# WP-5-delivery-media 片 3 A：媒体组聚合（2026-10-20）

范围与批准：`docs/plan/proposals/WP-5-delivery-media.md`「片 3 批准记录」。
判据：spec/03 §5.1 的媒体组语义；plan/00 §4.3 的 WP-3 / WP-4 / WP-5 出口判据。
本记录只说明本片的实施与验证，不宣布整个工作包完成。三图相册真机验收已通过，
改动尚未提交或发布，CI 尚未裁决。

## 一、实现

- 新增迁移 `0023_media_group_members.sql`，生产与测试迁移一致；不改已应用迁移。
  丙类表显式 `NO FORCE` / `DISABLE RLS`，`app_user` 只获 SELECT / INSERT。
  两端外键随事件级联，成员唯一、组内位置唯一，拒绝负位置及自引用。
  C7 白名单同步新增本表，double-pass 改为破坏本表的 RLS 标志。
- fanout 在扇出前按 `(receiver_identity_id, tg_chat_id, media_group_id)` 扫库。
  静默窗口满 2 秒才生成 `MEDIA_GROUP_COMPLETED`，关联按消息 id 排序；
  聚合事件、成员关联、原消息已扇出标记在一个事务提交。
  聚合与采集写入共用组级事务 advisory lock，聚合事务内再次检查窗口。
  不用 Redis；迟到消息按普通单条投递，不改已完成的组。
- 聚合事件沿用第一位成员的媒体类型，取第一条非空说明，文件大小取组内最大值，
  保证超限检查覆盖所有成员。规则只匹配一次；命中后成员同步标记 `matched_any`。
- pro 档 COPY / RESEND 为每个成员建下载 job，全部 READY 才整组发出；
  任一永久失败优先于其他成员的等待状态，整组判死，不部分投递。
  free 档只发文字，空说明用「[媒体组：N 项] 链接」。FORWARD 一次提交全部成员。
- Telegram 上传复用片 2 的流式代码：逐项 `uploadMedia` 得到可复用引用，
  再一次 `sendMultiMedia`；说明挂第一位，超长说明沿用拆分逻辑。
  窗口内只有一个成员时沿用 `sendMedia`，不依赖单成员 `sendMultiMedia`。
  每位成员从任务的 `telegram_random_id` 确定性派生正整数，组内不重复。

没有新增依赖。SQLx 缓存已重新生成，宏查询没有变化，`.sqlx/` 无差异。
spec/03 的建表清单静态核对通过，新增表已同步生命周期和审计说明。

## 二、本地测试

- `crates/db/tests/events.rs`：窗口内不扇出、并发聚合只生成一组、说明选择、混合媒体类型、
  成员顺序、迟到消息、身份/频道复合分组键、S/I 权限与级联删除。
  写入锁测试先观测 `pg_locks` 的真实 advisory 等待，再放开聚合事务；不是顺序伪并发。
- `crates/app/tests/media_relay.rs`：并发 fanout 后每租户只生成一个任务；
  中转 job 数与成员数一致；等待时不发送，全组就绪后 transport 收到全部有序成员；
  free 文字、FORWARD 成员列表、跨租户不可读取、成员命中标记、永久失败覆盖等待、
  空说明占位和单成员超限导致整组入队即 DEAD。
- L1：位置派生 random_id 的正数、唯一、确定性与上界回绕；
  Telegram 边界拒绝空组、超大组、数量不符、重复及非正 random_id。
- 所有数据库行为断言使用 `app_user`，不使用 SQLx 注入管理池冒充受限连接。
- 真机暴露的无文件名照片上传失败，补充 L1：照片缺失或空文件名时使用 `photo.jpg`，
  普通文档仍使用 `file`，有文件名时保留原名。单条与整组上传共用这一处理。

## 三、反证

逐项破坏后只跑对应用例，以下破坏都使测试失败，随后还原源码：

| 破坏 | 对应用例 |
|---|---|
| 聚合成员倒序 | `media_group_window_order_idempotency_and_late_arrival` |
| 写入绕开组锁 | `media_group_writes_wait_for_aggregation_lock` |
| 规则忽略聚合事件 | `media_group_fanout_rule_and_relay_keep_member_order` |
| 整组裁决忽略 DEAD | `media_group_dead_member_overrides_pending_member` |
| 中转档案读取倒序 | `media_group_fanout_rule_and_relay_keep_member_order` |
| 聚合文件大小取最小值 | `media_group_placeholder_and_oversized_member` |
| 同时移除预筛选与事务内窗口检查 | `media_group_window_order_idempotency_and_late_arrival` |

只删窗口预筛选时测试仍通过：事务内的二次检查仍拦住未到期成员。
这不是已通过的反证；同时破坏两处才检验到完整窗口性质。
最初用 `pg_stat_activity` 观察锁等待失败，改为可被测试角色读取的 `pg_locks` 后通过。

## 四、门禁

生产路径 `cargo run --bin tgm -- migrate`、连库的 `cargo sqlx prepare --workspace`、
`cargo clippy --workspace --all-targets -- -D warnings` 已通过。
完整 `scripts/ai-driver/ai-gates.sh` rc=0，输出「已执行的全部本地卡口通过」。
测试合计 336 passed / 0 failed / 12 ignored；新增本地用例通过，新增真机用例按预期忽略。
迁移幂等验证全部文件重新执行后库状态逐字节相同；`audit-rls` 与 `double-pass` 通过，
其中 C7 对 `media_group_members` 开启 RLS 后恰好这一条红，恢复后再次全绿。
本地通过不是 CI 裁决，改动尚未提交或发布。

真机修复后重跑完整 `scripts/ai-driver/ai-gates.sh`，rc=0，输出「已执行的全部本地卡口通过」。
测试合计 337 passed / 0 failed / 12 ignored，比修复前多出的 1 条是缺省照片文件名 L1；
`audit-rls` 为 18 条断言 0 条报红，`double-pass` 中 C7 仍是被单独打红的那一条。
本地 `cargo-deny` 为 0.20.2，CI 钉 0.19.1；脚本报告版本漂移，不能把本地结果替作 CI 裁决。

## 五、真机与边界

工具：`crates/app/tests/media_relay_live.rs` 的 `live_media_group_relay_end_to_end`，
带 `#[ignore]`，默认测试与 CI 不会访问 Telegram。测试频道为 `@tg_monitor_demo`，
源与目标同一个频道，发送 Bot 为 `@tg_monitor_1_bot`。

用户重新扫码后，`live_user_session_resume` 通过：`is_bot=false`、home DC 5、
读取最近消息成功。无 access_hash 的 ambient 频道读取报 `CHANNEL_INVALID`；
经用户名解析频道并把 access_hash 缓存回 session 后，媒体读取成功，不把前者当作采集失败。
session 与测试 JPEG 只存在仓库外 `~/.config/tgm-qr/`，目录权限 0700、文件权限 0600。
登录工具退出时删除当前二维码与链接，不在聊天或日志中输出 Token、api_hash、session 或密码。

首次真机中转在发送目标相册时失败，`delivery_logs` 记录 `PHOTO_EXT_INVALID`，
调用点为 `messages.uploadMedia`。源照片没有文件名，原上传默认名是 `file`；
在共享上传入口对缺省照片名改用 `photo.jpg` 后通过，单条图片也复用此修复。
测试说明同步改为含现有测试规则的关键词 `tgm-relay`，避免没有命中规则却误判媒体链路失败。
首次失败已经发出的源相册未删除；它不是中转重试产生的重复消息。

重跑 `live_media_group_relay_end_to_end`：1 passed / 0 failed，耗时 23.89 秒。
真实链路：Bot 发三图相册 → 用户身份读取并规范化 → 窗口内不投影 → 聚合、规则只生成一个任务 →
三个成员下载进 MinIO → Bot 整组投递 → 用户读回目标相册、核对成员数与说明；
相同 random_id 再发返回原消息。随后整组 FORWARD，重试仍返回原来的全部消息 id。

```text
[G] source_ids=[198, 199, 200] source_group=14340066181448661 tasks=1 members=3 first_target=201 target_group=14340066251301141 same_random_id=201 forwarded=[204, 205, 206]
```

本次覆盖真实 `sendMultiMedia`、用户身份下载、相册中转、相册说明、整组 FORWARD 与服务器重试去重。
不同媒体混合相册、超长说明相册和单成员相册尚未真测；本记录不把它们算成已验。
扫码使用的是测试工具，不宣布 WP-2 的 QR 出口判据通过。
