# ADR-0033：投递用数据 Bot 经 MTProto 登录，平台 `api_id` 只驱动租户自己的 Bot

状态：未定案（方案已选，平台 `api_id` 已申请，Bot 账号真机验证已通过；条款逐字核对未完成）

## 决定

- `delivery` 以租户数据 Bot 的身份发消息：用 Bot Token 走 MTProto
  `auth.importBotAuthorization`（grammers `bot_sign_in`），继续使用 raw
  `messages.sendMessage` / `messages.forwardMessages` 及其 `random_id`。
- 这条登录用的 `api_id` / `api_hash` 由平台申请、平台持有。它只用于驱动租户自己提交的
  Bot，不登录任何用户账号；租户 MTProto 采集身份仍按 [[ADR-0013]] 由租户自带。
- 发送身份限定为 `kind = 'bot'`。应用层在目标频道的创建与改发送身份两处拒绝其他类型；
  数据库层兜底另走迁移提案。

## 背景

规格把投递定为数据 Bot 发送，理由是 MTProto 用户账号一发消息，就从「监控」变成
「以用户身份行事」，落到官方条款的禁止行为上。已实现的发送路径却是 MTProto 用户 API，
因为 `random_id` 是唯一能在「平台已接受、本地未提交」的崩溃窗口里防重复的机制（G4）。

三个选项：

- **A Bot 经 MTProto**：守住「投递只走 Bot」，保留 `random_id` 防重，复用现有发送代码。
- **B Bot HTTP API**：守住合规，但 Bot API 没有 `random_id`，崩溃窗口内会重复，
  G4 只能退回本地幂等。
- **C 允许用户账号发送**：防重有，但推翻只读纪律与条款分析。

选 A。B 牺牲的是一条已定的出口判据，C 牺牲的是合规前提，A 牺牲的是一个新的平台凭据。

## 代价

- 平台多了一份自己的 `api_id`。[[ADR-0013]] 不代持的理由是「一个 `api_id` 绑住所有租户」：
  这里同样成立 —— 任何租户的 Bot 滥用若被 Telegram 记到这个 `api_id` 上，
  全平台投递一起停。区别只在于登录的是 Bot 而不是用户，Bot 本身受 Bot 限流约束，
  但 `api_id` 层面的连坐是否存在，没有找到官方说明。
- 条款「你自己的 `api_id`」按「申请主体与使用者同一」解读。平台申请、平台程序使用，
  字面成立；但被驱动的 Bot 属于租户，这一层没有逐字核对过。定案前要核对。
- Bot 的 MTProto session 要按身份加密落库，`identity_secrets` 里同一行同时要有 Token
  与 session，现有列只有一个 `secret_ciphertext`。是否需要新列或新表，未定。
- `random_id` 去重在 Bot 账号上没有实测，Telegram 文档只按账号描述。G4 的真机验收
  必须在 Bot 账号上做，不能借用户账号的结果。

## 相关

[[ADR-0011]]、[[ADR-0013]]、[[ADR-0032]]。
提案 `docs/plan/proposals/WP-5-delivery-sender-channel.md`；
落地在 `spec/02-总体架构.md`（投递角色）、`spec/06-合规与凭据责任.md`（`api_id` 归属）、
`spec/08-账号接入.md`（只读纪律）。
