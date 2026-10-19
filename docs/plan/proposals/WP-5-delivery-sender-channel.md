# WP-5-delivery-sender-channel：投递通道与发送身份类型

## 任务与判据

任务 id：`WP-5-delivery-sender-channel`。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-5（后台任务在进程重启后继续）
与 §三 G4；发送身份见 `docs/design/spec/02-总体架构.md` §六（投递 = 数据 Bot），
`docs/design/spec/08-账号接入.md` §1.4（采集身份只读，投递一律走 Bot 身份）；
解密权限见 `spec/05` §七与 ADR-0032。

按 `AGENTS.md` §五第 3、4、5 条（凭据与发送面改动、需要 ADR 或 A 类变更、需要 Telegram
真机），本任务升级，`tgm serve --role delivery` 暂不接入运行链。

## 发现

`delivery` 是运行链里唯一还没接上的角色。接线本身不难，难在下面两条事实互相冲突：

1. **规格定的发送通道是数据 Bot。** `spec/02` §六 的角色表写投递由「数据 Bot（按
   `sender_identity_id`）」完成；`spec/08` §1.4 写「投递一律走 Bot 身份」，
   理由是 MTProto 账号一旦发消息，就从监控变成以用户身份行事，落到官方条款的禁止行为上
   （`spec/06` §1.1）。
2. **已实现的发送路径是 MTProto 用户 API。** `crates/tg/src/client.rs` 的
   `copy_text` / `forward_message` 用 raw `messages.sendMessage` /
   `messages.forwardMessages`，并依赖它们的 `random_id` 做服务端去重。这是
   `WP-5-delivery-random-id` 获批的方向，也是目前唯一能在「平台已接受、本地未提交」
   的崩溃窗口里防重复的机制（G4）。

两者叠在一起的后果：要把现有 `DeliveryWorker` 接上 `TelegramDeliveryTransport`，
`--role delivery` 需要一个**以 Bot 身份登录的 MTProto 客户端**（`auth.importBotAuthorization`），
或者干脆用 MTProto 用户账号发送。前者规格里没有讨论过；后者直接违反 §1.4。

另外核实到一处数据层缺口：`create_target_channel` 只校验 `sender_identity_id`
属于本租户，**不校验 `kind = 'bot'`**。租户今天就能把一个 MTProto 采集身份填成发送身份。
delivery 一旦接上，这就是一条以用户身份发消息的路径。

还有一处：`random_id` 去重域是「同一账号、同一目标 peer」。Bot 账号通过 MTProto
发送同样受这条规则约束 —— 但这一点没有在 Bot 账号上实测过，Telegram 文档也只按账号描述。

## 选项

| 选项 | 做法 | 守住 §1.4 | G4 防重 | 代价 |
|---|---|---|---|---|
| A Bot 经 MTProto | delivery 用 `api_id`/`api_hash` + Bot Token 走 `bot_sign_in`（grammers 已有），复用现有 raw 发送与 `random_id` | 是 | 有协议依据，需 Bot 账号实测 | 需要平台自己的 `api_id`（ADR-0013 只覆盖租户 MTProto 身份）；Bot 的 MTProto session 也要按身份加密落库；需 ADR |
| B Bot HTTP API | delivery 用 Bot API `sendMessage` / `copyMessage` / `forwardMessage`，与控制 Bot 同一种 HTTP 客户端 | 是 | **无**：Bot API 没有 `random_id`，崩溃窗口内会重复 | 改写 transport；G4 判据要调整或另做对账；`WP-5-delivery-random-id` 的成果大半作废 |
| C 允许 MTProto 用户发送 | 不限制发送身份类型，按现状接线 | **否** | 有 | 推翻 §1.4 与 `spec/06` 的条款分析，需要法务与产品确认 |

## 建议

选 A，并补两条约束：

- **发送身份限定为 Bot。** 在应用层（`create_target_channel` / `update_target_channel`）
  拒绝非 `kind = 'bot'` 的 `sender_identity_id`；若要数据库兜底，另行提交迁移提案。
- **平台 `api_id` 的归属写进 ADR。** 它只用于以平台托管的方式驱动**租户自己的 Bot**，
  不代表任何用户登录。这一点与 ADR-0013 和 `spec/06` §1.1 的「申请主体与使用者同一」
  需要逐字核对，不能默认成立。

理由：A 是唯一同时满足「不以用户身份发消息」和「崩溃窗口内不重复」的方案，而且复用
现有发送代码与 `random_id` 数据模型。B 守住了合规，却把 G4 的防重能力降回本地幂等，
恰好是 `WP-5-delivery-restart` 提案已经论证不足的那条路。

## 需要批准

1. 选项 A / B / C 之一。
2. 若选 A：平台 `api_id` 用于 Bot 的 MTProto 登录，是否接受，以及谁去 my.telegram.org 申请；
   ADR 由我起草。
3. 是否先单独落地「发送身份限定为 Bot」的应用层校验（不涉及迁移，可独立合入）。

获批前不接入 `--role delivery`，不改发送通道，不改 `target_channels` 的数据约束。

## 验收（获批后）

- `--role delivery` 接入运行链，按 `sender_identity_id` 分 worker；凭据经
  `load_identity_secret` 运行期解密。
- L2：受限角色下，非 Bot 发送身份建目标被拒；delivery 进程强杀重启后任务继续
  （沿用 `crates/app/tests/delivery_restart.rs` 的形状）。
- 真机：同一任务在请求前、平台接受后两个时点强杀重试，目标频道只出现一条消息（G4）。

## 批准记录（2026-10-19）

- 选项：**A**（Bot 经 MTProto）。ADR 草稿 `docs/design/decisions/ADR-0033-投递Bot经MTProto登录.md`，
  状态未定案。
- 「发送身份限定为 Bot」的应用层校验：批准单独落地，已实现。
  `crates/db/src/repo.rs` 的 `check_sender_identity` 在创建与改发送身份的同一事务内查 `kind`，
  非 Bot 返回 `CONDITION_INVALID`（400）；控制台的发送身份下拉只列 `kind=bot`。
  L2：`target_router_crud_is_tenant_scoped_and_logically_deletes` 以 `app_user` 连接，
  断言 mtproto 身份建目标与改发送身份都被拒、库内不变。
  反证：把 `Some("bot")` 换成接受任意 kind，该测试在 201 ≠ 400 处变红；已还原。
- 平台 `api_id`：已由人申请（2026-10-19），经 `TGM_PLATFORM_API_ID` / `TGM_PLATFORM_API_HASH`
  注入，仅 delivery 允许出现。
- Bot session：**不落库**（批准）。delivery 启动时用 Token 现登录，session 只在内存；
  不改 `identity_secrets`、不加迁移。成立前提两条，须 Bot 账号真机验证：
  重启频率下 `auth.importBotAuthorization` 不触发限流；`random_id` 去重按 Bot 账号生效、
  不随 auth key 变化（否则 G4 不成立，退回新表方案并另交迁移提案）。

## 实现记录（2026-10-19，分支 `codex/WP-5-delivery-runtime`）

- `tgm serve --role delivery` 接入：`crates/app/src/delivery_runtime.rs` 扫描发送身份，
  每身份解密 Token → `TelegramClient::connect_bot_sender`（`bot_sign_in`，丢弃更新通道）→
  既有 `DeliveryWorker`。
- 候选 `list_delivery_sender_candidates`：`kind = 'bot'`、非 banned/unauthorized、有 Token，
  且被启用目标引用或还有未完成任务。历史数据里被填成发送身份的 mtproto 不会起 worker。
- Token 格式不对、登录返回的 Bot id 与 Token 前缀不符、Telegram 拒绝 Token：
  `mark_bot_sender_unauthorized` 同一事务内转 `unauthorized` 并写审计 `identity.unauthorized`。
- 预检：`TGM_PLATFORM_API_ID/HASH` 为 delivery 必填，其他角色出现即报多配。
- 未验证：真机 `bot_sign_in`、G4 两个时点强杀、媒体 COPY/RESEND（仍判永久失败）。

## 真机验收步骤（工具：`crates/app/tests/delivery_live.rs`）

准备：测试用 Bot（不用正式租户的 Bot），一个测试频道并把该 Bot 设为可发消息的管理员；
本地 compose 栈；KEK 用开发 `EnvKek`。凭据只在本机 shell 里 `export`，不写进任何文件，
不贴进对话或证据。

```bash
export DATABASE_URL=postgres://tgm_test:tpw@127.0.0.1:55432/tgm_test_control
export SQLX_OFFLINE=true
export TGM_PLATFORM_API_ID=...        # 平台 api_id
export TGM_PLATFORM_API_HASH=...      # 平台 api_hash
export TGM_LIVE_BOT_TOKEN=...         # 测试 Bot 当前 Token
export TGM_LIVE_TARGET_CHAT=-100...   # 测试频道 chat id

# V1 登录与限流（默认 5 次，可设 TGM_LIVE_LOGIN_REPEAT）
cargo test -p tgm-app --test delivery_live live_bot_login_repeat -- --ignored --nocapture
# V2 G4：请求前 / 平台已接受后两个时点强杀，重启用全新 session 重发
cargo test -p tgm-app --test delivery_live live_delivery_g4_kill_restart -- --ignored --nocapture
# V3：在 BotFather 里 /revoke 重置 Token，把旧 Token 填进来
export TGM_LIVE_REVOKED_BOT_TOKEN=...
cargo test -p tgm-app --test delivery_live live_revoked_token -- --ignored --nocapture
```

判据：

- V1：每轮 `login=ok bot_id_matches=true`；出现 `FLOOD_WAIT` 即「session 不落库」不成立。
- V2：每个 phase 一行 `[V2] ... dedupe=...`。`same-message-id` 或 `random-id-duplicate` 表示未重复；
  `DUPLICATE-SENT` 表示 G4 不成立，退回新表方案。另需**人工**看测试频道：每个 `tgm-g4 <phase> <random_id>`
  只出现一次。`random-id-duplicate` 且任务未到 `DONE` 时测试会红，说明去重有效但应用层要补
  「重复即已送达」的收口。
- V3：`exit=Ok(Unauthorized) status=unauthorized audits=1`。

缺任一变量时测试直接失败，不会空跑成绿。输出只含 Bot id 是否匹配、错误码、消息 id 与任务状态，
可原样贴回来做证据；跑完删掉测试频道里的测试消息，在 BotFather 里作废测试 Token。

## 真机验收结果（2026-10-19）

V1 / V2 / V3 全部通过，证据 `docs/plan/evidence/WP-5-delivery真机验收-2026-10-19.md`。
`random_id` 去重按 Bot 账号生效、不随 auth key 变化（after-send 强杀后新 session 重发返回同一消息 id，
频道人工核对无重复），「session 不落库」的两条前提在本次样本下成立，不需要退回新表方案。

## 后续：`CHANNEL_INVALID` 改判永久失败（2026-10-19，分支 `codex/WP-5-channel-invalid-permanent`）

真机验收里 Bot 不在目标频道时，MTProto 返回 `CHANNEL_INVALID`，`DeliveryErrorKind::from_code`
把它归为可重试，任务会按 30 秒 / 5 分钟 / 30 分钟白跑三轮才进死信。这个错误要等人把 Bot
重新加进频道才会消失，重试没有意义，所以改判为 `Permanent`，直接进死信，与已有的
`CHAT_WRITE_FORBIDDEN`、`CHANNEL_NOT_FOUND` 同档。`CHANNEL_PRIVATE`（Bot 被封禁或频道
对它不可见）同理一并加入。

测试：`delivery::tests::error_codes_classify_permanent_and_flood_wait_failures` 补了两码判永久、
`TELEGRAM_CLIENT_ERROR` 仍判可重试的断言。反证：从名单删掉 `CHANNEL_INVALID` 后恰好这一条红，已还原。
人工重新把 Bot 加回频道后，死信任务走现有的控制面重试入口恢复，本次不新增自动恢复。

## 定案记录（2026-10-19，分支 `codex/ADR-0033-clause-check`）

条款逐字核对完成，证据 `docs/plan/evidence/ADR-0033-条款逐字核对-2026-10-19.md`。人工批准定案，
ADR-0033 状态改为已定案。核对新增一条硬约束（Bot 开发者条款 §5.2(f)：被封 Bot 的任务不换号重发），
由 L2 `delivery_tasks_are_not_claimed_by_another_bot_of_same_tenant` 守住。
