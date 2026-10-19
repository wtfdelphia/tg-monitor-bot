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
- 仍待定：平台 `api_id` 由谁去 my.telegram.org 申请；Bot session 的存放形状
  （`identity_secrets` 现只有一个 `secret_ciphertext`，需迁移提案）。两项未定前不接入 `--role delivery`。
