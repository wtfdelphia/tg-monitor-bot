# WP-5 delivery 常驻长跑（2026-10-19）

任务 id：`WP-5-delivery-sender-channel` 后续项。判据出处：`docs/design/plan/00-里程碑与工期.md`
§三 G4、§4.3 WP-5；`docs/design/ops/00-部署与运行时.md` §六；ADR-0033 代价节
「重启风暴下的限流没有测过」。

工具：`bins/tgm/tests/delivery_soak.rs`（`#[ignore]`，凭据只经环境变量，判据 S1–S4 见文件头）。

```text
测试版本：a616c1f + 工作区未提交改动（分支 codex/WP-5-delivery-soak-tool）
进程：真实 tgm serve --role delivery 子进程，profile=development，KEK 为开发 EnvKek
测试账号：测试 Bot tg_monitor_1_bot（id 8869608883），非租户 Bot
目标：测试频道 @tg_monitor_demo，Bot 为管理员、can_post_messages=true
平台应用：平台自有 api_id（值不入证据）
数据库：sqlx::test 临时库，子进程以 app_user 连接
```

## 一、30 分钟长跑结果

参数：每 20 秒放一条任务，每 600 秒交替 SIGTERM / SIGKILL 重启一次。

```text
[soak] minutes=30 tasks=90 done=90 distinct_message_ids=90 dead=0 starts=3 logins=3 login_flood_waits=0
       sigterm=2 sigkill=1 slowest_sigterm_exit=105.969685ms failures=[] identity_status=offline unauthorized_audits=0
test result: ok. 1 passed; 0 failed; finished in 1803.37s
```

| 判据 | 结果 |
|---|---|
| S1 不丢不重 | 90 条全部 DONE，90 个不同的 `telegram_message_id` |
| S2 无死信 | `dead=0`，失败日志为空 |
| S3 SIGTERM 优雅退出 | 2 次 + 收尾 1 次，最慢 106ms，均以 0 退出 |
| S4 身份未被误标 | `identity_status=offline`，`identity.unauthorized` 审计 0 条 |

频道人工核对（`tgm-soak #1..#90` 每条只出现一次）：测试环境提供方于 2026-10-20 确认通过。

## 二、重启频繁时登录被限流

同一 Bot 在几分钟内反复 `bot_sign_in`，第 4 次左右开始被限流。三轮观察（重启间隔 4–120 秒）：

```text
FLOOD_WAIT caused by auth.importBotAuthorization (value: 254)
FLOOD_WAIT caused by auth.importBotAuthorization (value: 246)  … 依次 201 / 155 / 111（同一窗口倒计时）
bot_sign_in 被限流，等满服务器给的时间再重试, flood_wait_seconds: 186
bot_sign_in 被限流，等满服务器给的时间再重试, flood_wait_seconds: 66
bot_sign_in 被限流，等满服务器给的时间再重试, flood_wait_seconds: 305
```

另有一次 1 分钟小样本：3 次启动只有 1 次登录成功，日志里没有限流告警。推测是 grammers
对短时限流自动静默等待，未验证。

结论：ADR-0033「session 不落库、重启即重新登录」在正常停旧起新下成立（本次 600 秒间隔 3 次登录
无限流）；重启频繁时投递会停几分钟。是否把 Bot session 落库另起提案
`docs/plan/proposals/WP-5-delivery-bot-session.md`。ADR-0033 已定案，正文不改。

## 三、过程中发现并已修复

第一次冒烟 SIGTERM 后 30 秒内未退出（S3 失败）。原因两条：`connect_bot_sender` 在限流等待里
不响应退出信号；`DeliveryRuntime::run` 退出时无上限地等每个发送身份。修复：

- 登录与限流等待都 `select!` 退出信号；`FLOOD_WAIT` 时等满服务器给的秒数再重试，
  不再 30 秒冷却后立即重撞（`TelegramClientError::flood_wait`）。
- 退出时最多等 `SHUTDOWN_GRACE`（30 秒，ops/00 §六）让在途发送身份收尾，超时放弃。
  正确性不受影响：卡在 `SENDING` 的任务由下次启动回收，`random_id` 防重。

修复后的限流轮次里，SIGTERM 4 次均在约 110ms 内以 0 退出。

## 卡口

完整 `scripts/ai-driver/ai-gates.sh`（本批改动，基于 `dev` `a616c1f`）：rc=0，
测试 302 passed / 0 failed / 5 ignored（新增的 ignored 是本长跑测试，需真机凭据），
`double-pass 通过`、`已执行的全部本地卡口通过`。

## 四、未验证

- 媒体 COPY / RESEND、FORWARD、真实 `KmsKek` 下的运行期 Decrypt。
- 多个 Bot 同时发送、长于 30 分钟的运行。
- control-plane / bot-gateway 两个 HTTP 角色的 SIGTERM 进程级测试。
