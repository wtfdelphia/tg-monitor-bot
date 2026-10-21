# WP-2-live-updates：实时收消息落库与 `catch_up` 补齐真机实测

状态：已批准并完成（2026-10-21）。U2 暴露 grammers-session 缺陷，按方案 A（本地补丁）修复后 U1–U3 全部 pass，证据见 `docs/plan/evidence/WP-2-live-updates-2026-10-21.md`。

## 任务与判据

任务 id：`WP-2-live-updates`。

判据出处：
- `docs/design/plan/00-里程碑与工期.md` §4.1 WP-2 内容「消息接收与落库」；
- `docs/design/spec/08-账号接入.md` §1.3：`catch_up: true` 负责补齐断线期间的更新；`updates_state` 随 session 一起持久化；
- `docs/plan/evidence/WP-2-出口判定-2026-10-21.md` §四 的前两条缺口，以及「60 秒定时落库后进程重启恢复」这一条。

按 `AGENTS.md` §五第 5 条升级（Telegram 真机、真凭据），不直接实现。

## 现状（2026-10-21 核对代码）

- 生产循环 `run_identity`（`crates/app/src/mtproto_worker.rs`）用 `TelegramClientConfig::new(api_id)` 建连，`catch_up` 默认是 true。`next_incoming` 收到的消息由 `canonical_event` 转换，再经 `insert_canonical_event_leased` 落库，唯一键 `uq_canonical` 负责去重。这条链路目前只有 L2，用的是伪造的 `IncomingMessage`。
- 现有真机工具（`mtproto_live.rs`、`media_relay_live.rs`）全部调了 `without_catch_up()`。读消息走的是 `read_messages`，没有走实时更新流。
- session 每 60 秒落库一次，正常退出前也会再落一次。T5 只验证了「落库后能恢复」，没有验证「进程被硬停后，从上一次定时落库的位置续上」。
- 测试 Bot `@tg_monitor_1_bot` 的 token 有效，在 `@tg_monitor_demo` 里是管理员，有发帖权限（2026-10-21 用 Bot API `getMe` 和 `getChatMember` 核对过）。`TGM_LIVE_BOT2_TOKEN` 调用返回 400，看起来已失效；本任务用不到它。

## 范围

不新增迁移，不改 RLS / GRANT / 审计，不加依赖，不改生产代码。只做以下两件事：

- 在 `crates/app/tests/mtproto_live.rs` 新增 ignored 用例；
- 新增证据文件。

如果真机暴露生产缺陷，最小修复在批准范围内，需要补 L1 或 L2 并写进证据；修复如果命中 §五 第 1–4 条，就停下另写提案。

用例 `live_updates_realtime_catch_up_and_hard_stop`：用一个扫码得到的 session，经 `save_session` 加密落库。测试用 `sqlx::test` 临时库，连接角色为 `app_user`，KEK 用开发 EnvKek。worker 全程用 `run_identity` 和默认配置，`catch_up` 为 true。测试消息由测试 Bot 经 Bot API 发进频道，正文带本次运行的随机 nonce。

| 项 | 操作 | 判据 |
|---|---|---|
| U1 实时收消息 | worker `online` 后，Bot 发 1 条新消息，再把它编辑一次 | 30 秒内 `canonical_events` 出现这条消息的 `MESSAGE_CREATED`（`edit_version=0`），正文与发出的一致；编辑后出现一条 `MESSAGE_EDITED`，`edit_version` 等于 Telegram 返回的 `edit_date` |
| U2 正常退出后补齐 | 发停止信号，worker 正常退出（退出前落库）。离线期间 Bot 发 2 条，并编辑其中 1 条。然后用库里的 session 重启 `run_identity` | 重启后 60 秒内，离线期间的 2 条新消息和那 1 次编辑全部落库，各只有一条；U1 的消息没有新增行 |
| U3 硬停后从定时落库续上 | worker 在线超过 1 个落库周期（默认 60 秒，等 75 秒）后，直接 abort 任务，不发停止信号，也就不做退出前落库。将租约过期，模拟另一实例接管。离线期间 Bot 发 2 条，然后重启 | 重启后 60 秒内这 2 条各只有一条；U1、U2 的消息没有新增行。同时记录 abort 前后 `identity_secrets` 密文是否在落库周期内更新过 |

去重口径：按 `(receiver_identity_id, tg_chat_id, tg_message_id, event_type, edit_version)` 统计，每组恰好一条，与 `uq_canonical` 一致。补齐时如果 Telegram 把已经落库的更新又投了一遍，会被唯一键拦下而不新增行。这同样算 pass，证据里记下重投次数。

脱敏要求：只记录时间、消息 id、事件类型、计数。不记录手机号、`api_hash`、session、bot token、二维码内容、密码。

## 需要你配合的事

```text
1 用测试用户账号重新扫码（上次 user.session 已删）。我用 qr_login 的 live_user_qr_login
  写出新的 session；2FA 密码沿用 ~/.config/tgm-qr/2fa.txt 交付，读完即删。
2 测试账号需仍是 @tg_monitor_demo 的成员（T5 时是）。
3 用例会让测试 Bot 在频道里发 5 条带 tgm-live-updates 前缀的消息，并编辑其中 2 条。
  结束后用 Bot API deleteMessage 删除这些消息；如果你希望保留它们以便核对，告诉我。
4 测试结束后删除 user.session，并在手机端移除该设备（或保留给下一次真机）。
```

## 不在本次范围

- 运行中定期授权校验识别撤销：要在 worker 在线期间撤销会话，会让 session 失效，放到下一次真机窗口末尾再做；
- 租户通知（spec/08 §五第 2 条）：依赖通知通道设计，另行提案；
- 混沌清单（spec/08 §4.5）。

## 需要你决定的事

1. 是否批准上述范围：在 `mtproto_live.rs` 新增 ignored 用例，新增证据文件。
2. 测试消息结束后是否删除，默认删除。
3. 是否把「运行中撤销」作为可选的 U4 加在 U3 之后。加上的话 session 会失效，下次真机需要重新扫码。

## 执行记录（2026-10-21）

扫码登录：第 2 代二维码，`MigrateTo 2 -> 5`，2FA 一次通过。

- U1：pass，跑了两次。新消息 0.5 秒落库，编辑最长 2 秒落库，`edit_version` 等于 `edit_date`，正文一致。
- U2：fail，跑了两次。worker 正常退出前落库，离线期间发 2 条消息，重启后 60 秒内都没有落库。
- U3：前置 U2 失败，没有执行。

### 根因（grammers trace 日志）

1. 退出前落库的 `updates_state` 是对的，测试频道 `channels: [{ id: 4431202271, pts: 502 }]`。
2. 重启后 `catch_up` 正常发起 `updates.getDifference`。服务端在 `other_updates` 里返回 `UpdateChannelTooLong { channel_id: 4431202271, pts: Some(505) }`，这个 505 已经包含离线期间的消息。
3. `grammers-session 0.10.0` 的 `MessageBoxes::process_updates` 处理 `ChannelTooLong` 时，用 `set_entry` 把本地 pts 从 502 直接**覆盖**成 505，然后才开始取频道差量。
4. 结果 `updates.getChannelDifference` 从 pts 505 开始取，返回 `ChannelDifferenceEmpty`，离线期间 503–505 的消息被静默跳过，没有报错，也没有日志告警。

对照：单独诊断时 session 里还没有这个频道的 pts（`channels: []`），频道消息经 `forcing update` 正常补回。所以只要 session 里已经存过频道 pts，补齐就会丢消息，这正是生产常态。

crates.io 最新的 `grammers-session 0.11.0`（2026-10-15）和 Codeberg master 里这段逻辑相同，升级解决不了。

这一条和 spec/08 §1.3「`catch_up: true` 负责断线期间的更新补齐」不符。按 RK-02 的口径，以实测为准。

### 测试频道清理

测试 Bot 发的消息都已删除。核对 message id 245–255 时，245 也被删掉了。245 早于本次运行发的第一条（246），来源不明，可能不是本次测试发的。

### 修复路线（已批准：A）

2026-10-21 选定方案 A：把 `grammers-session 0.10.0` 拷贝到 `vendor/`，用 `[patch.crates-io]` 接入，只改 `ChannelTooLong` 分支。依赖来源由 crates.io 改为本地路径；`cargo deny check` 通过（`sources ok`）。补丁说明见 `vendor/README.md`。
