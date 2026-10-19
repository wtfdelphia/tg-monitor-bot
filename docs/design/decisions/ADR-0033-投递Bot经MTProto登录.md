# ADR-0033：投递用数据 Bot 经 MTProto 登录，平台 `api_id` 只驱动租户自己的 Bot

状态：已定案（2026-10-19）

## 决定

- `delivery` 以租户数据 Bot 的身份发消息：用 Bot Token 走 MTProto
  `auth.importBotAuthorization`（grammers `bot_sign_in`），继续使用 raw
  `messages.sendMessage` / `messages.forwardMessages` 及其 `random_id`。
- 这条登录用的 `api_id` / `api_hash` 由平台申请、平台持有。它只用于驱动租户自己提交的
  Bot，不登录任何用户账号；租户 MTProto 采集身份仍按 [[ADR-0013]] 由租户自带。
- 发送身份限定为 `kind = 'bot'`。应用层在目标频道的创建与改发送身份两处拒绝其他类型；
  数据库层兜底另走迁移提案。
- 投递任务绑定创建时的发送身份。一个 Bot 被封或 Token 被拒后，它的任务不换给别的 Bot 发出，
  身份标为不可用并留审计。
- Bot 的 MTProto session 只在进程内存里，不落库；进程重启就用 Token 重新登录。

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
  这里同样成立，而且是条款明文。Bot 开发者条款写明，他人使用你的凭据所做的行为视同你所为，
  所以任何租户 Bot 经平台 `api_id` 发出的滥用消息，都算在平台头上，全平台投递一起停。
  工程上只能收窄面：只驱动 Bot、目标限于租户自己的频道、被封的 Bot 不换号重发。
  连坐本身消除不了。
- 条款「你自己的 `api_id`」按「申请主体与使用者同一」解读。平台申请、平台程序使用，
  字面成立；条款的主语是应用，没有要求被驱动的 Bot 的所有者另申 `api_id`。
  官方 MTProto 文档也写明 Bot 登录「仍需提供你的 API ID」。
- 平台使用的是租户提交的 Bot Token，形态上就是条款里「他人提供的凭据」。条款禁止的是
  借此绕过封禁或审核，所以「被封的 Bot 不换号重发」是这条决定能成立的前提，不是可选项。
- session 不落库，代价是每次进程重启都要重新 `bot_sign_in`。真机样本（连续 5 次）未触发限流，
  重启风暴下的限流没有测过。
- `random_id` 去重在 Bot 账号上按账号生效、不随 auth key 变化，这一点已经真机实测；
  样本只有一个 Bot、一个频道。

## 相关

[[ADR-0011]]、[[ADR-0013]]、[[ADR-0032]]。
提案 `docs/plan/proposals/WP-5-delivery-sender-channel.md`；
条款核对 `docs/plan/evidence/ADR-0033-条款逐字核对-2026-10-19.md`；
真机验收 `docs/plan/evidence/WP-5-delivery真机验收-2026-10-19.md`；
落地在 `spec/02-总体架构.md`（投递角色）、`spec/06-合规与凭据责任.md`（`api_id` 归属）、
`spec/08-账号接入.md`（只读纪律）。
