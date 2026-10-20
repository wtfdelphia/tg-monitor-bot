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

## 片 2 实施提案（2026-10-20）

片 1 已合入 dev（PR #24）。片 2 会新增表、迁移和依赖，并改动 GRANT 与 `eng/01` 的配置清单，
命中 `AGENTS.md` §五第 1、3、5 条，所以先写到这里等批准，暂不实现。

### 范围

只覆盖 pro 档 COPY / RESEND 的单条媒体中转。FORWARD 保持现在的语义：发送 Bot 能读到源消息才转，
读不到就按永久失败处理，不悄悄改成中转，因为 FORWARD 会带来源署名，中转不带，两者语义不同。
free 档按批准记录的口径，只发文字或 FORWARD，不进入本片的中转路径。
媒体组和对象清理留到片 3。

### 数据（新迁移 0022）

1. 新表 `media_relay_jobs`。每条 canonical event 对应一个下载任务，多个租户命中同一条消息时
   共用一次下载：

```sql
CREATE TABLE IF NOT EXISTS media_relay_jobs (
  canonical_event_id   BIGINT PRIMARY KEY REFERENCES canonical_events(id) ON DELETE CASCADE,
  receiver_identity_id BIGINT NOT NULL REFERENCES identities(id) ON DELETE CASCADE,
  status          TEXT NOT NULL DEFAULT 'PENDING'
                  CHECK (status IN ('PENDING','DOWNLOADING','READY','DEAD')),
  media_archive_id BIGINT REFERENCES media_archive(id),
  attempt_count   INTEGER NOT NULL DEFAULT 0,
  next_retry_at   TIMESTAMPTZ,
  last_error_code TEXT,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_relay_due ON media_relay_jobs (receiver_identity_id, next_retry_at)
  WHERE status IN ('PENDING','DOWNLOADING');
```

   表里没有 `tenant_id` 列，和 `canonical_events` 一样属于丙类，不启用 RLS，需要同步改
   spec/03 §1.3 的丙类清单和 0009 的注释口径。因为没有 `tenant_id`，A1–A9 的主查询
   不会覆盖它，这是丙类本来的边界。隔离靠应用层：delivery 只能经自己租户的
   `delivery_tasks → tenant_events → canonical_event_id` 查到 job。
   另一种做法是把状态列直接加在 `canonical_events` 上，好处是不增加丙类表，
   代价是重试状态混进事件表，不推荐。

2. `delivery_tasks` 不加列。caption 拆两条时，第二条的 `random_id` 用第一条的值确定性派生：
   `(telegram_random_id ^ 0x5a5a_5a5a_5a5a_5a5a) & i64::MAX`，结果为 0 时取 1。
   片 1 已证明同一个 `random_id` 重发会被服务器去重，所以重试时两条都整组重发，
   不需要记录「第一条已成功」这类中间进度。第二条的 `message_id` 不落库（`delivery_logs` 没有对应列）；`telegram_message_id`
   仍指向媒体那一条（编辑目标消息时用它）。
   派生值与同一 `(sender, target)` 下其他任务的 `random_id` 碰撞的概率约为 2^-63，
   这里留一条 `ponytail:` 注释，触发条件是「出现编辑第二条文字的需求」，届时改为独立列。

3. GRANT：`media_relay_jobs` 授 `SELECT, INSERT, UPDATE` 给 `app_user`，`media_archive` 已有授权。

### 链路

```text
rule 生成投递任务（同一事务）
  pro 档 + COPY/RESEND + media_type 非空且非 TEXT
    file_size > 50 MB      → 该任务直接 DEAD MEDIA_TOO_LARGE，不建 job
    否则                    → INSERT media_relay_jobs ... ON CONFLICT DO NOTHING
  free 档 + COPY/RESEND + 媒体 → 快照标记 text_only，发送时把 caption 当正文；
                                 没有 caption 时正文用占位说明（文案见下）

mtproto-worker（持有 receiver 身份的租约）
  每个续租周期顺带扫一次自己身份的到期 job
  get_messages_by_id 重新取消息（拿新的 file_reference）
  → iter_download，边下边 sha256 → S3 multipart（8 MB 分片，内存里最多一片）
  → 按 content_hash upsert media_archive；同 hash 的对象未过期时，中止本次 multipart 并复用旧对象
  → job READY；可重试错误按 30s/5min/30min 退避，第 4 次失败 DEAD

delivery
  任务带媒体：查 job
    READY      → S3 GetObject 流 → upload_stream → sendMedia（带 random_id）
                 caption 不超过 1024 字：一条发完
                 超过 1024 字：先发不带 caption 的媒体，再 sendMessage 发全文（派生 random_id）
    未就绪     → 推后 next_retry_at，不增加 attempt_count；等待超过 30 分钟判 DEAD MEDIA_RELAY_TIMEOUT
    job DEAD   → 任务 DEAD，错误码沿用 job 的
```

- **上传形态**：PHOTO 用 `InputMediaUploadedPhoto`，其他类型用 `InputMediaUploadedDocument`，
  带上原 `mime_type` 和 `file_name`。视频和音频的时长、尺寸等属性目前没有采集，
  第一版不带，Telegram 可能显示为普通文件或缺少时长。这一点在真机验收里看，留 `ponytail:` 注释，
  触发条件是「视频不能内联播放」，届时改为在 job 里保存原 document attributes。
- **跳过下载**：MTProto 侧在下载之前拿不到 `content_hash`，spec/02 §4.2 第 3 条
  「命中 `media_archive` 跳过下载」在本片做不到，只能在下载完成后去重存储。
  按 document id 跳过下载留到以后，留 `ponytail:` 注释。
- **长度**：`MEDIA_CAPTION_TOO_LONG` 和 `MESSAGE_TOO_LONG` 加入永久失败码。现在的文本投递遇到
  超过 4096 字时会白白重试三次，这是同一个问题，顺手修掉。拆两条时，如果全文本身超过 4096 字，
  判 DEAD `MESSAGE_TOO_LONG`，不截断原文（`eng/02` 的约束）。
  长度按 HTML 解析之后的纯文本计算。片 1 只用 BMP 字符测过，按 UTF-16 码元还是按 code point
  计数尚未验证，L1 测试用 emoji 补上这一条。
- **占位文案**：free 档的媒体消息没有 caption 时，正文用「[媒体：<media_type>] <来源链接>」，
  照常走模板渲染。

### 依赖与配置

- 根 `Cargo.toml` 新增
  `aws-sdk-s3 = { version = "=1.152.0", default-features = false, features = ["behavior-version-latest", "default-https-client", "rt-tokio"] }`。
  1.152.0 与已钉的 `aws-sdk-kms 1.123.0` 同一天发布（2026-10-01），它对 `aws-smithy-runtime-api ^1.19.0`、
  `aws-smithy-types ^1.8.1`、`aws-runtime ^1.10.0`、`aws-types ^1.6.0` 的要求与根清单的钉版一致，
  `aws-smithy-runtime ^1.16.0` 与 Cargo.lock 现有版本一致（以上都已在 crates.io 核对）。
- **S3 凭据单独配置，不走 AWS 默认凭据链。** 默认链已经被 KMS 占用，`eng/01` 禁止并存的凭据来源。
  MinIO 或 OCI 对象存储的 key 和 AWS KMS 的 key 不是同一套，共用一条链会出现一方遮蔽另一方。
  新增两个配置项 `TGM_S3_ACCESS_KEY_ID` 和 `TGM_S3_SECRET_ACCESS_KEY`（Secret，只配给 mtproto-worker
  和 delivery），以及可选的 `TGM_S3_REGION`（默认 `us-east-1`）。S3 客户端使用显式的静态凭据提供者，
  端点取 `TGM_S3_ENDPOINT`，采用 path-style。同时改 `eng/01` §一和 config 预检。
- compose 的 MinIO 目前用镜像默认 root 凭据。L2 测试在测试里自己建 bucket，不改 `compose.yaml`。

### 测试

- L1：caption 分条判定（1024 边界和 emoji）、派生 `random_id` 恒为正且与原值不同、
  free 档 text_only 渲染、新错误码分类。
- L2（compose 里的 PG 和 MinIO）：
  - rule 入队时只给 pro 档的媒体建 job，free 档不建，同一事件多个租户只建一个 job；
  - 超过 50 MB 直接 DEAD；
  - 用假的 Telegram 下载流跑 worker 下载，写入 S3 并 upsert `media_archive`，
    同 hash 第二次下载复用旧对象；
  - delivery 在 job 未就绪时推后且 attempt_count 不变，超时判 DEAD；
  - 以受限角色连接时，跨租户的 delivery 查不到别人的 job。
- 反证：每个 L2 判据都删掉对应的一行实现，确认对应的测试变红。
- 真机：需要一个 **MTProto 用户账号** session 作为采集身份（G1 的 QR 真机项还没过，本仓目前
  没有真实用户 session）。拿不到时，片 2 只能做到 L2 加「Bot 作为下载方」的真机，
  用户账号下载与 `file_reference` 过期重取留作片 2 的未完成项写进证据，不宣称 WP-5 媒体出口达成。

### 片 2 需要批准

1. 新表 `media_relay_jobs`（丙类，同步改 spec/03 §1.3），还是改为在 `canonical_events` 上加列。
2. caption 拆两条时第二条用派生 `random_id`、`delivery_tasks` 不加列（推荐），还是新增独立列。
3. S3 使用独立的静态凭据 `TGM_S3_ACCESS_KEY_ID` / `TGM_S3_SECRET_ACCESS_KEY`，不走 AWS 默认链。
4. 新增依赖 `aws-sdk-s3 =1.152.0`。
5. pro 档 FORWARD 在 Bot 读不到源消息时仍判永久失败，不回落到中转。
6. 真机验收用哪个 MTProto 用户账号：现在扫码登录一个测试账号，还是先只做 L2 加 Bot 下载方真机。

### 片 2 批准记录（2026-10-20）

1. 用新表 `media_relay_jobs`（丙类，同步改 spec/03 §1.3）。
2. 第二条用派生 `random_id`，`delivery_tasks` 不加列。
3. S3 用独立的静态凭据，不走 AWS 默认凭据链。
4. 新增依赖 `aws-sdk-s3 =1.152.0`。
5. pro 档 FORWARD 在 Bot 读不到源消息时仍判永久失败，不回落到中转。
6. 真机验收：现在扫码登录一个测试用户账号，作为采集身份。

### 片 2 落地状态（2026-10-20）

迁移 0022、审计断言 C7、中转 worker 和 delivery 等待链路已经实现。L1、L2、反证和真机验收的结果见
`docs/plan/evidence/WP-5-delivery-media-relay-2026-10-20.md`。真机验收中发现用户账号用 ambient
引用读频道会报 `CHANNEL_INVALID`，已改为优先使用 session 缓存的 `access_hash`。
片 3（媒体组、对象清理）未开始。

## 片 3 实施提案（2026-10-20）

### 现状（2026-10-20 核对 `9a19e49`）

- 采集侧已记 `media_group_id`：mtproto-worker 写 `grouped_id`，bot-gateway 写 Bot API 的
  `media_group_id`。但之后整条链路都不看它：fanout 逐条扇出，rule 逐条匹配，相册里每一张
  命中都单独投一次。spec/01 那一行写的正是要避免的情况：「不做会让 9 图相册触发 9 次」。
- `CanonicalEventType::MediaGroupCompleted` 枚举和 CHECK 都已存在，但没有代码生成这种事件。
- 仓库里没有 Redis 客户端依赖。`TGM_REDIS_URL` 已在配置清单里，但没有代码使用。
- `media_archive.object_expires_at` 片 2 已经写成 3 天，但没有代码删过期对象，也没有代码删
  180 天前的档案记录。对象存储里的文件只增不减。
- `staging/` 前缀：片 2 下载失败或撞上已有 hash 时会删除临时对象，但删除失败只忽略
  （`let _ = store.delete(..)`），会留下孤儿对象。

### 范围

本片做两件事，可以分成两个提交：

**A. 媒体组聚合**（首发项，spec/03 §5.1）

**B. 对象存储清理**（spec/04 §一 `media_archive` 行；只做这一行，其他表的留存期仍归 WP-8）

不做：编辑与删除事件、其他表的清理任务、媒体组里单张超限的部分投递（见 A.5）。

### A. 媒体组聚合

**A.1 聚合放在哪一层。** 放在 fanout 之前、canonical 层，不放在 rule 层。理由：

- spec/03 §5.1 写的是「生成 `MEDIA_GROUP_COMPLETED`，该事件持有组内全部 message_id，规则只跑一次」，
  聚合事件本身就是 canonical 事件；
- 放在 rule 层的话，每个租户都要各自聚合一遍，还要处理「同组消息分到两批」的竞态；
- 放在 canonical 层，多个租户共用一次聚合，和片 2「一个事件一个 job」的形状一致。

**A.2 窗口用 PG 判定，不依赖 Redis。** spec/02 §1.1 的纪律是「Redis 只是唤醒信号，
每个 Worker 都必须能在 Redis 完全丢失时靠扫库恢复」。静默窗口如果只放在 Redis 里，
Redis 一丢，组就永远聚合不了（或者重复聚合）。所以判定直接查库：

```text
fanout 扫到一条带 media_group_id 的 MESSAGE_CREATED：
  同组（receiver_identity_id, tg_chat_id, media_group_id）最新一条的 created_at
    距今 < 2s  → 跳过，本轮不标 fanned_out，下轮再看
    距今 ≥ 2s  → 在一个事务里：
                 插入一条 MEDIA_GROUP_COMPLETED（tg_message_id = 组内最小 id）
                 组内全部 MESSAGE_CREATED 标 fanned_out（它们本身不扇出、不投递）
                 聚合事件照常扇出
```

`spec/02` §1.1 键名清单里的 `mgroup:{identity}:{group_id}` 本片不用：仓库目前没有 Redis 客户端，
fanout 本来就是轮询扫库，2 秒窗口靠扫库判定就够了。这是对 spec/02 键名清单的偏离，
同步在 spec/03 §5.1 的流程图里改写，并在 spec/02 的键名清单那一行注明「首发不用」。
这属于变更分级的 C 类（实现细节），不写 ADR。

幂等：聚合事件同样受 `uq_canonical` 约束（`event_type` 不同于组内单条），两个 fanout 实例
同时判定时只有一个能插进去，另一个 `ON CONFLICT DO NOTHING` 后重新读取已有的聚合事件。
组内某条迟到（超过 2 秒才到）时，它会自己凑成一个新组，还是被并进已经完成的组？
这里**按迟到消息单独成组处理**：插入时发现同组已有 `MEDIA_GROUP_COMPLETED`，就把这条迟到消息
当作普通单条消息扇出。代价是迟到的那一张会单独投一次；好处是已投出的相册不会被改写。

**A.3 数据：需要一张关联表。** 聚合事件要「持有组内全部 message_id」，现有 `canonical_events`
是一行一条消息，放不下组内成员。新增丙类表：

```sql
CREATE TABLE IF NOT EXISTS media_group_members (
  group_event_id     BIGINT NOT NULL REFERENCES canonical_events(id) ON DELETE CASCADE,
  member_event_id    BIGINT NOT NULL REFERENCES canonical_events(id) ON DELETE CASCADE,
  position           INTEGER NOT NULL,          -- 按 tg_message_id 升序，决定相册顺序
  PRIMARY KEY (group_event_id, member_event_id),
  UNIQUE (member_event_id)                      -- 一条消息只属于一个组
);
```

丙类表，形状和 `media_relay_jobs` 一样：显式 NO FORCE + DISABLE RLS，只 GRANT S/I 给 app_user，
随 `canonical_events` 级联删除。表数从 28 增加到 29，需要同步 spec/03、spec/04 和各处计数，
审计断言 C7 的丙类表清单也要加上它。

另一种做法是在 `canonical_events` 上加一列 `group_event_id`。这样不用新表，但要改一张已有的大表，
而且组内成员和聚合事件之间是一对多关系，用关联表表达更直接。**推荐新表**。

**A.4 规则怎么跑。** 聚合事件的 `text` / `caption` 取组内第一条非空的 caption
（Telegram 相册通常只有一张带说明），`media_type` 记 `MEDIA_GROUP`？不行：`rules.media_type`
的 CHECK 里没有这个值，加值就要改表。所以**聚合事件的 `media_type` 取组内成员的类型**
（全是 PHOTO 就是 PHOTO，混合就是 DOCUMENT 以外的第一种，混合规则见下面的批准项 3）。
关键词只对聚合后的说明匹配一次，命中就投整组，不命中整组都不投。

**A.5 投递。**

- **FORWARD**：一次 `messages.forwardMessages`，`id` 和 `random_id` 都传组内全部成员。
  `random_id` 从任务的 `telegram_random_id` 按位置派生（和片 2 的 `split_text_random_id` 同一个思路）。
- **COPY / RESEND（pro 档中转）**：每个成员一个 `media_relay_jobs`（主键仍然是成员的
  `canonical_event_id`，片 2 的表不改）。delivery 等组内全部 job 就绪后，调用一次
  `messages.sendMultiMedia`：先逐个 `messages.uploadMedia` 拿到 `InputMedia`，再一起发出。
  说明挂在第一个成员上，超过 1024 时沿用片 2 的拆分规则（相册后面再发一条全文）。
- **free 档**：只发文字（片 1 批准口径），正文是聚合后的说明，没有说明时用占位
  「[媒体组：N 项] 链接」。
- 组内任何一个成员永久失败（源消息被删、超过 50 MB）时，**整组判死**，不部分投递。
  部分投递会让目标频道出现残缺的相册，而且重试时很难保证不重复。

**A.6 测试。**

- L1：聚合事件的 `media_type` 合并规则；位置派生 `random_id` 恒为正且两两不同；
  占位文案。
- L2：
  - 3 条同组消息在窗口内只生成 1 个聚合事件，且只投递 1 次；
  - 窗口未到时不扇出；
  - 两个 fanout 并发判定，只生成一个聚合事件；
  - 迟到消息单独扇出，不改已有的组；
  - 组内一个 job 判死，整组任务判死；
  - pro 档整组就绪后，transport 收到的载荷带全部成员且顺序正确。
- 真机：用户账号作为采集身份，在测试频道里由 Bot 1 发一个 3 张图的相册（`sendMultiMedia`），
  验证整组中转后以相册形式发出，并且同一组 `random_id` 重发时返回原消息。
  需要用户账号重新扫码登录，片 2 用的 session 已经删除。

### B. 对象存储清理

**B.1 做法：定时任务，不用 S3 生命周期规则。** 生命周期规则只能按对象创建时间删除，
但片 2 的去重会**复用**还没过期的对象（同 hash 第二次下载时保留旧对象），所以对象的实际
有效期由 `media_archive.object_expires_at` 决定，而不是由创建时间决定。按创建时间删除，
可能删掉一个刚被复用、delivery 正要读取的对象。另外，MinIO 和各家 S3 兼容存储对生命周期规则的
支持程度也不一样。

**B.2 任务内容**（spec/04 §1.1 的清理纪律：分批、每批不超过 5000 行、批间 200 ms、写 `system_logs`）：

```text
1. object_expires_at < now() 的档案：先删对象，再把 object_key / object_expires_at 置空。
   顺序不能反：先置空再删对象，删除失败就留下孤儿对象，而且再也查不到。
   删对象返回 404 当作成功（幂等）。
2. first_seen_at < now() - 180 天的档案行：物理删除。
   前提是没有 media_relay_jobs 引用它；有引用就跳过，等 job 随 canonical_events 级联删除后再清。
3. staging/ 前缀下超过 1 天的对象：列出后删除（清理片 2 删除失败留下的孤儿）。
```

**B.3 跑在哪个角色里。** 放在 mtproto-worker 里需要租约，放在 delivery 里会有多个实例。
推荐**放在 fanout 角色里**，每小时跑一次：fanout 本来就不持有 Telegram 身份，部署上通常只跑一个
实例；即便跑多个，B.2 的三步也都是幂等的。fanout 现在没有 S3 配置，需要在 eng/01 §二 把
`TGM_S3_*` 的适用角色加上 fanout。

**B.4 测试。**

- L2（MinIO）：过期对象被删且档案的 key 被置空；未过期的不动；先删对象再置空（破坏顺序后测试变红）；
  180 天前、仍被 job 引用的档案不删；`staging/` 孤儿对象被清理；`system_logs` 写入 `cleanup.done`。
- 不需要真机。

### 依赖与配置

- 不新增依赖：`aws-sdk-s3` 已有 `list_objects_v2`；grammers 0.10.0 已有
  `messages.uploadMedia` 与 `messages.sendMultiMedia` 的原始 TL。
- 新迁移 0023 `media_group_members`。
- eng/01 §二：`TGM_S3_*` 的适用角色加上 fanout。

### 片 3 需要批准

1. 媒体组聚合放在 canonical 层，2 秒静默窗口用扫库判定、首发不用 Redis
   （spec/02 键名清单里 `mgroup:` 那一行注明首发不用；按 C 类处理，不写 ADR）。
2. 新表 `media_group_members`（丙类，表数增加到 29），还是改为在 `canonical_events` 上加
   `group_event_id` 列。
3. 混合类型相册（例如图片加视频）的 `media_type`：取第一个成员的类型（推荐，简单、可预期），
   还是规则 `media_type` 只要匹配组内任一成员就算命中。
4. 组内任一成员永久失败时整组判死，不部分投递。
5. 迟到消息（窗口结束后才到）单独成组投递，不并入已经完成的组。
6. 对象清理用定时任务（不用 S3 生命周期规则），跑在 fanout 角色里，fanout 增加 `TGM_S3_*` 配置。
7. 本片按 A、B 两个提交、一个 PR 提交；还是 B 先单独开 PR（B 不依赖真机，可以先合）。
8. 真机：A 需要用户账号重新扫码登录作为采集身份。

### 片 3 批准记录（2026-10-20）

用户批准「片 3 需要批准」全部 8 条。有备选的几条按提案推荐执行：

1. 媒体组聚合放在 canonical 层，2 秒静默窗口靠扫库判定，首发不用 Redis；按 C 类处理，不写 ADR。
2. 新表 `media_group_members`（丙类，表数增加到 29）。
3. 混合类型相册的 `media_type` 取第一个成员的类型。
4. 组内任一成员永久失败时整组判死，不部分投递。
5. 迟到消息单独成组投递，不并入已完成的组。
6. 对象清理用定时任务，跑在 fanout 角色里，fanout 增加 `TGM_S3_*` 配置。
7. B（对象清理）不依赖真机，先单独开 PR；A（媒体组聚合）随后另开 PR。
8. A 的真机验收需要用户账号重新扫码登录，作为采集身份。

### 片 3 B 落地状态（2026-10-20）

B（对象清理）已实现，按批准记录第 7 条单独开 PR。证据：
`docs/plan/evidence/WP-5-delivery-media-cleanup-2026-10-20.md`。与 B.4 的差异：
`system_logs` 没有写入，清理结果只进 tracing（`app_user` 写不进 `tenant_id IS NULL` 的行），
在 spec/04 §1.1 注明了缺口。A（媒体组聚合）未动。

### 片 3 A 实施状态（2026-10-20）

在 `codex/WP-5-delivery-media-group-a` 基于 PR #29 合并后的 `dev` 实施。
已写迁移 0023、聚合事务、规则与中转接线、整组发送、L1 / L2 和真机工具；
同步 C7、double-pass 及规格。局部测试、反证与完整门禁已通过；用户重新扫码后，
三图相册中转、整组 FORWARD 与相同 random_id 重试去重真机通过。
真机暴露的 `PHOTO_EXT_INVALID` 已在共享上传入口修复，并补充缺省照片文件名的 L1 检查。
记录：`docs/plan/evidence/WP-5-delivery-media-group-2026-10-20.md`。
修复后完整门禁通过：337 passed / 0 failed / 12 ignored。
尚未提交、发布或经 CI 裁决；混合类型、超长说明及单成员相册未真测。
