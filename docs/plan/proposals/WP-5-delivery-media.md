# WP-5-delivery-media：媒体投递（COPY / RESEND 带媒体、跨身份中转）

## 任务与判据

任务 id：`WP-5-delivery-media`。

判据出处：
- `docs/design/decisions/ADR-0031-首发包含媒体中转.md`：首发包含媒体，不得静默降级为只转文本。
- `docs/design/spec/01-产品需求.md`：§3.3 投递动作、§6.1 pro 档单文件 50 MB、free 档不中转、
  §8.2 「优先转发而非中转」。
- `docs/design/spec/02-总体架构.md` §4.2：跨身份中转、流式、边下边算 `content_hash`、
  命中 `media_archive` 跳过下载、对象存储保留 3 天。
- `docs/design/eng/02-本地环境与迁移.md`：`caption_length_max` 1024，越界不得截断原文。
- `docs/design/plan/00-里程碑与工期.md` §三 G1：媒体中转可行性。

按 `AGENTS.md` §五第 1、3、5 条（可能改表、新增依赖进保护文件 `Cargo.toml`、
需要 Telegram 真机）升级，不直接实现。

## 现状（2026-10-20 核对代码）

- **FORWARD** 已实现（`crates/tg/src/client.rs` 的 `forward_message`，raw `messages.forwardMessages`
  带 `random_id`），但只有真机文本样本，没有媒体样本。
- **COPY / RESEND** 只发文本：`media_type` 非空且不是 `TEXT` 时，直接返回
  `MEDIA_COPY_UNAVAILABLE` / `MEDIA_RESEND_UNAVAILABLE`，按永久失败进死信。
- 采集侧：mtproto-worker 记录 `media_type` / `file_name` / `file_size` / `mime_type`，
  `content_hash`、`raw_file_id` 恒为空；bot-gateway 记录 `raw_file_id`（Bot API file_id）。
  两边都**不下载媒体**。`media_archive` 没有任何代码读写。
- 媒体组聚合（`MEDIA_GROUP_COMPLETED`，2s 静默窗口）没有实现：相册现在按单条消息逐条处理。
- 没有 S3 客户端依赖。`TGM_S3_ENDPOINT` / `TGM_S3_BUCKET` 已配给 mtproto-worker 与 delivery，
  但没有代码使用。

## 需要先定的事实问题

身份关系决定了能走哪条路，而这两条路目前都没有真机证据：

1. **发送 Bot 不在源频道里时，FORWARD 能不能用。** ADR-0033 之后，发送身份恒为租户 Bot；
   pro 档采集身份是 MTProto 用户账号。发送 Bot 通常不是源频道成员，`forwardMessages` 的
   `from_peer` 它很可能解析不到（预期 `CHANNEL_PRIVATE`，现在会直接判死信）。
   若属实，spec/02 §4.2 的「同身份快路径」在本架构下几乎不会发生，FORWARD 只对 Bot 自己
   也在源频道里的配置有效。
2. **free 档（数据 Bot 收）与「不中转」。** spec/02 写 free 档采集身份是「租户自建数据 Bot」，
   但 bot-gateway 现在按部署只接一个 `TGM_DATA_BOT_IDENTITY_ID`。如果收和发是同一个 Bot，
   它本来就在源频道里，FORWARD 可行，不需要中转；如果是两个 Bot，§6.1「不中转」意味着
   free 档的媒体消息只能以文本投递或不投。这是产品口径，需要确认，不是实现能决定的。
   另外 bot-gateway 存的 `raw_file_id` 是 Bot API 的 file_id，MTProto 发送不能直接用，
   同 Bot 复用也要先解码，片 1 一并验证。

## 建议切片

一次做完风险太大（下载、对象存储、上传、聚合、caption、清理，每一项都要真机）。拆成三片，
每片单独提案批准记录、单独 PR：

### 片 1：真机可行性 spike（不改 schema，不加依赖）

- `#[ignore]` 真机工具，回答上面两个事实问题：
  - 发送 Bot 不在源频道时，`forwardMessages` 报什么；
  - 发送 Bot 是成员时，媒体能否带 `random_id` 正常 FORWARD；
  - MTProto 账号 `iter_download` 下载一条 ≤ 50 MB 的媒体，Bot 经 MTProto `upload_stream`
    再 `messages.sendMedia`（带 `random_id`）发到目标频道，看内存峰值与耗时。
- 产出：证据文件，加上对 spec/02 §4.2 的结论（快路径是否成立）。这一片不进生产代码。

### 片 2：跨身份中转主链路

- 新增依赖 `aws-sdk-s3`（与已钉的 `aws-config` 同一组版本），写进根 `Cargo.toml`。
- **下载放在哪一侧**：发送 Bot 拿不到采集账号的 `file_reference`，所以下载只能在采集侧
  （mtproto-worker）做。规格写的是「接收侧流式下载」，但没写**什么时候**下载。建议
  **命中规则、生成投递任务之后再下载**，而不是收到就下：pro 档 30 个订阅、每天 6000 事件里
  只有约 5% 命中，收到就下等于 20 倍流量。代价是 L2 去重（`content_hash`）只对命中的消息
  生效，与 spec/03 「L2 文件级去重」的字面意思不符，需要一并确认。
- 下载触发：新增「待中转」状态或任务队列，mtproto-worker 持租约下载 → 边下边 sha256 →
  S3 multipart → 写 `media_archive`。delivery 发现快照里有对象引用就从 S3 流式读，
  `upload_stream` 后 `sendMedia`。对象没好时任务退避，不判失败。
- schema：投递快照里加 `media.object_key` / `content_hash` / `mime_type` / `file_name`，
  0019 的 CHECK 不限制多余键，不需要改；但「待中转」状态和 `media_archive` 的归属、清理
  可能需要新列或新表，片 2 提案里再给出具体 DDL。
- caption 超过 1024 时：不截断，任务判永久失败并带明确错误码（`CAPTION_TOO_LONG`），
  控制台可见。另一种做法是拆成「媒体 + 第二条文本」，但会多一条消息、要两个 `random_id`，
  一并请决定。
- 单文件上限 50 MB（pro），超限判永久失败 `MEDIA_TOO_LARGE`；free 档不中转。

### 片 3：媒体组聚合与清理

- Redis 静默窗口聚合 `MEDIA_GROUP_COMPLETED`，`messages.sendMultiMedia` 一次发整组。
- 对象存储 3 天过期清理（S3 生命周期规则或定时任务），`media_archive` 180 天保留。

## 需要批准

1. 是否按三片推进、先做片 1。片 1 只加 `#[ignore]` 真机工具和证据，需要：
   一个 MTProto 采集账号的 session（或在测试频道里由 Bot 自己当源）、测试 Bot、测试频道。
2. free 档媒体口径：以文本投递，还是只在 Bot 是源频道成员时 FORWARD，还是直接不投。
3. 下载时机：命中后下载（建议，省流量但 L2 只覆盖命中消息），还是收到就下载（符合规格字面）。
4. caption 超 1024：判永久失败，还是拆两条。

## 批准记录（2026-10-20）

1. 按三片推进，先做片 1；提供测试 Bot 1（源频道管理员）与测试 Bot 2（非成员）。
2. free 档媒体：只发文字；只有 Bot 本身就在源频道里时才 FORWARD。free 档不中转。
3. 下载时机：命中规则后再下载。L2 文件级去重只覆盖命中的消息，接受这一偏离。
4. caption 超过 1024：拆成「媒体 + 一条文字」两条发送，每条各用一个 `random_id`。

## 片 1 结论（2026-10-20）

证据：`docs/plan/evidence/WP-5-delivery-media-spike-2026-10-20.md`。

- 发送侧可行：Bot 经 MTProto 上传 50 MB 再 `sendMedia` 成功，同一 `random_id` 重发返回同一
  `message_id`；FORWARD 媒体同样去重。
- 下载为分片流，50 MB 上传加下载的峰值 RSS 为 17.7 MiB，可以接 S3 multipart，不落盘。
- caption 上限按字符计 1024，服务器超长报 `MEDIA_CAPTION_TOO_LONG`。
- 发送 Bot 不是私有源频道成员时，读不到源消息也不能 FORWARD（`CHANNEL_INVALID`），所以
  pro 档的媒体以中转为常态，与 spec/02 §4.2 一致。

### 据此修订片 2

- caption：发送前按字符数判断。不超过 1024 时，用一条 `sendMedia` 带 caption；超过时，
  先发不带 caption 的媒体，再 `sendMessage` 发全文（`eng/02` 的 `message_length_max` 为 4096；文本投递目前没有超长拆分逻辑，片 2 提案里一并说明）。
  两条各自用一个由投递任务确定性派生的 `random_id`，重试时两条都能去重；第一条已成功、
  第二条失败时，只重试第二条。快照需要记两条各自的 `telegram_message_id`，具体 DDL 在片 2 提案里给出。
- 下载：mtproto-worker 在投递任务生成之后才下载（用户账号持租约，边下边 sha256，写 S3）。
  下载方是用户账号，`file_reference` 过期重取在片 2 实测。
- free 档：媒体消息按文字投递（caption 作为正文，没有 caption 时投递占位说明，文案在片 2 定）；
  投递动作为 FORWARD 且发送 Bot 能读到源消息时才 FORWARD。free 档不写对象存储。
- 快路径只保留「发送 Bot 能读源消息时直接 FORWARD」一种，不做 Bot API `file_id` 复用。
