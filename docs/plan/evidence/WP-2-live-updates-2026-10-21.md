# WP-2 实时收消息落库与 `catch_up` 补齐真机实测

记录日期：2026-10-21

任务 id：`WP-2-live-updates`（提案 `docs/plan/proposals/WP-2-live-updates.md`，已批准）

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.1 WP-2「消息接收与落库」；`docs/design/spec/08-账号接入.md` §1.3；
`docs/plan/evidence/WP-2-出口判定-2026-10-21.md` §四的前两条缺口，以及「60 秒定时落库后进程重启恢复」这一条。

脱敏：本文只记时间、计数和事件类型，不记手机号、`api_hash`、session、bot token、二维码内容和密码。
测试账号是人工持有的测试用户账号，使用平台 `api_id`；测试频道 `@tg_monitor_demo`，测试消息由测试 Bot 经 Bot API 发送。

## 一、结论

| 项 | 结果 | 工具输出原文（补丁后最终一次） |
|---|---|---|
| U1 实时收消息与编辑 | pass | `[U1] new_after=109.589054ms edit_after=2.370563ms edit_version=edit_date` |
| U2 正常退出后 `catch_up` 补齐 | pass（修复后） | `[U2] exit_before=Shutdown caught_up_after=639.290737ms chat_rows_added=2 edit_as_separate_row=false created_has_edited_text=true` |
| U3 硬停后从定时落库续上 | pass | `[U3] periodic_save=true unchanged_by_abort=true caught_up_after=1.641531551s chat_rows_added=2 expected_added=2 exit=Shutdown` |

全程使用生产循环 `run_identity` 与默认 `MtprotoWorkerConfig`，`catch_up` 为 true。临时库由 `sqlx::test` 创建，连接角色为 `app_user`，KEK 是开发 EnvKek。
每个 `(message_id, event_type, edit_version)` 都断言恰好一行，与 `uq_canonical` 的列一致。

## 二、过程

扫码登录：用 `live_user_qr_login` 扫第 2 代二维码，触发 `MigrateTo 2 -> 5`，2FA 一次通过。

命令（`#[ignore]`）：

```bash
cargo test -p tgm-app --test mtproto_live live_updates_realtime_catch_up_and_hard_stop -- --ignored --exact --nocapture
```

1. 补丁前跑两次：U1 两次都 pass；U2 两次都 fail，离线期间发的 2 条消息在重启后 60 秒内都没有落库。
2. 根因定位：开 grammers trace 日志后重跑，另写了一个独立诊断用例做对照，诊断用例不提交。结论记在提案「执行记录」：
   `getDifference` 返回 `updateChannelTooLong { pts }`，`grammers-session 0.10.0` 用它覆盖了已知频道的本地 pts（502 → 505），
   于是 `getChannelDifference` 从 505 开始取，返回 `ChannelDifferenceEmpty`。
3. 修复：方案 A，`vendor/grammers-session` 本地补丁（说明见 `vendor/README.md`）。L1 两条：
   `test_channel_too_long_keeps_known_channel_pts` 补丁前红、补丁后绿；`test_channel_too_long_seeds_unknown_channel_pts` 确认本地不认识的频道仍按上游行为建条目。
   上游原有的 19 条测试全部保持通过。
4. 补丁后跑两次，都是 pass。第一次的 `rows_added` 统计了该身份的全部对话，测试账号在其他群里收到的消息也混进来了，计数虚高（U2 5、U3 8）。
   改成只统计测试频道后跑第二次，即上表结果。

## 三、各项细节

- U1：worker `online` 后 Bot 发 1 条消息，约 0.1 秒落为 `MESSAGE_CREATED`，正文一致。
  编辑后约 2 毫秒落为 `MESSAGE_EDITED`，`edit_version` 等于 Telegram 返回的 `edit_date`，正文是编辑后的版本。
  补丁前两次的新消息落库时间约 0.5 秒。
- U2：worker 收到停止信号后正常退出，退出前落库。离线期间 Bot 发 2 条，并编辑其中 1 条。重启后约 0.64 秒补齐 2 条新消息。
  离线期间的那次编辑没有单独产生 `MESSAGE_EDITED` 行：补齐返回的是编辑后的正文，落在 `MESSAGE_CREATED` 行里（`created_has_edited_text=true`），
  内容没有丢，但这条消息「曾被编辑」的事件不会出现在事件流里。U1 那条消息没有新增行。
- U3：worker 在线 75 秒，跨过一个 60 秒的落库周期，`identity_secrets.updated_at` 前移（`periodic_save=true`）。随后直接 abort 任务，
  不发停止信号，也就不做退出前落库；abort 前后 `updated_at` 相同（`unchanged_by_abort=true`）。手动让租约过期，模拟另一实例接管。
  离线期间 Bot 发 2 条，重启后约 1.6 秒补齐，测试频道只新增这 2 行，U1、U2 的消息没有新增行，最后正常退出。

## 四、清理

- 测试 Bot 发的消息在每次运行结束时用 `deleteMessage` 删除，最终一次 `sent=5 deleted=5`。
- 补丁前第一次失败后，按 message id 245–255 逐个补删时，245 也被删掉了。245 早于本次第一条（246），来源未查明。
- `user.session`、渲染二维码用的临时 Python 包、诊断用例和 trace 日志都已删除。测试账号在手机端仍留有一个已授权设备，可以手动移除。

## 五、未验证与边界

- 离线期间的编辑不产生独立的 `MESSAGE_EDITED` 事件（见 U2）。依赖「编辑事件」的规则，在 worker 离线窗口内会漏掉编辑。
  这是 Telegram 差量的语义，不是补丁引入的问题；本文只记录现象，没有修复。
- 只测了一个频道，离线窗口只有几秒。长时间离线时差量会分页（`ChannelDifferenceTooLong`、`limit=100`），这条路径没有测。
- 运行中周期授权校验识别撤销、租户通知、混沌清单，仍按 `WP-2-出口判定-2026-10-21.md` §四处理。
- 补丁依赖 `vendor/` 的拷贝；升级 grammers 时需要按 `vendor/README.md` 核对上游是否已修复。

## 六、门禁

见对应 PR。本地已过 `cargo clippy --workspace --all-targets -- -D warnings` 与 `cargo deny check`（advisories、bans、licenses、sources 全部 ok）；
完整门禁结果以本地 `ai-gates.sh` 和 CI 为准。
