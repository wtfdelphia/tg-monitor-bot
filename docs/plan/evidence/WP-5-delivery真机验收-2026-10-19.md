# WP-5 delivery 真机验收（2026-10-19）

任务 id：`WP-5-delivery-sender-channel`。判据出处：`docs/design/plan/00-里程碑与工期.md` §三 G4、
§4.3 WP-5；`docs/design/decisions/ADR-0033-投递Bot经MTProto登录.md` 代价一节列出的两条前提；
提案 `docs/plan/proposals/WP-5-delivery-sender-channel.md` 的「真机验收步骤」。

工具：`crates/app/tests/delivery_live.rs`（`#[ignore]`，凭据只经环境变量）。

```text
执行时间：2026-10-19 16:20–16:46（CST）
测试版本：05eaf7f + 工作区未提交改动（分支 codex/WP-5-delivery-runtime）
测试账号：测试 Bot tg_monitor_1_bot（id 8869608883），非租户 Bot
目标：测试频道 @tg_monitor_demo（chat id -1004431202271），Bot 为管理员、can_post_messages=true
平台应用：平台自有 api_id（值不入证据）
环境：local；compose 栈 kms-kek；KEK 为开发 EnvKek；数据库为 sqlx::test 临时库，以 app_user 连接
```

## 一、结果

| 编号 | 验证项 | 结果 | 观察 |
|---|---|---|---|
| V1 | 连续 5 次 `bot_sign_in` | pass | 5 轮 `login=ok bot_id_matches=true`，无 `FLOOD_WAIT` |
| V2 | G4 两时点强杀、重启用新 session 重发 | pass | 见下 |
| V3 | Token 经 BotFather `/revoke` 作废后启动 | pass | `exit=Ok(Unauthorized) status=unauthorized audits=1` |

V2 输出（原样）：

```text
[V2] phase=before-send label="tgm-g4 before-send 1792398597700002509" first=- restarted=ok:7 task_status=DONE telegram_message_id=Some(7) dedupe=single-send
[V2] phase=after-send label="tgm-g4 after-send 1792398612447424805" first=ok:8 restarted=ok:8 task_status=DONE telegram_message_id=Some(8) dedupe=same-message-id
```

人工核对（测试环境提供方，2026-10-19）：频道里只有 3 条消息——权限探测 1 条（message_id 6，
Bot API 发出，不属于 G4 用例）、`before-send` 1 条（7）、`after-send` 1 条（8）。无重复。

## 二、结论

- **`random_id` 去重按 Bot 账号生效，不随 auth key 变化。** after-send 场景里第一个进程已被平台接受
  （消息 8）后强杀；重启进程从空 session 重新 `bot_sign_in`（新 auth key），以同一 `random_id` 重发，
  Telegram 返回同一条消息 8，未产生新消息。这是 ADR-0033 代价一节「`random_id` 去重在 Bot 账号上没有实测」
  的实测答案，也是「session 不落库」成立的前提之一。
- **重启频率下登录未触发限流**（V1，5 次，约 3 分钟内）。这是前提之二在小样本下成立；
  更高频的重启风暴没有测。
- 重发时 Telegram 直接返回原消息，不返回 `RANDOM_ID_DUPLICATE`；任务正常到 `DONE`，
  不需要为该错误码补应用层收口。
- Token 作废后身份转 `unauthorized`，审计 `identity.unauthorized` 恰好一条。

## 三、过程中的发现

第一次跑 V2 两个阶段都返回 `CHANNEL_INVALID`，Bot API `sendMessage` 同时报
`Forbidden: bot is not a member of the channel chat`：Bot 当时实际上不在频道里。测试环境提供方
重新添加管理员后，Bot API 探测发送成功，再跑通过。代码未改。
`CHANNEL_INVALID` 当前被 `DeliveryErrorKind::from_code` 归为可重试；它在「Bot 被移出目标频道」时也会出现，
是否应判永久失败留给 WP-5 后续评估，本次不改。

## 四、未验证

- 媒体 COPY / RESEND（仍判永久失败）、FORWARD 动作、`FLOOD_WAIT` 真实触发与退避。
- `tgm serve --role delivery` 整进程长时间运行（本次走的是同一套 `run_delivery_once` 与
  `connect_bot_sender`，但没有起常驻进程）。
- 真实 `KmsKek` 下 delivery 的运行期 Decrypt。
- 条款逐字核对（ADR-0033 定案的另一项前置）。

## 五、凭据处置

- 测试 Bot Token 曾出现在对话记录中；验收后已由测试环境提供方在 BotFather `/revoke`，V3 用的正是这个作废的旧 Token。
- 平台 `api_hash` 曾出现在对话记录中；是否在 my.telegram.org 重新生成由持有人决定。
- 本机临时凭据文件（仓库外，权限 600）已在验收后删除；证据中不含 Token、`api_hash` 或 session。
