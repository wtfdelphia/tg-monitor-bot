# Telegram 多租户内容监控与分发平台

> 文档状态：**最初的架构草案，已被 [`docs/design/`](design/README.md) 的文档集取代**  
> 原始日期：2026-09-18 · 本注更新：2026-09-21  
> **实现基线不是本文**，是 `docs/design/` —— 入口见 [`docs/design/README.md`](design/README.md)。
> 本文保留为需求来源与决策溯源（早期文档引用它时称「主文档草案」），
> 不再维护。与 `docs/design/` 冲突时**一律以后者为准**。
>
> 中间还存在过一版 `docs/rust/`（14 份）。它已被 `docs/design/` 全量承载并删除，
> 所以本文原先指向 `rust/xx` 的那些引用现在都指向 `docs/design/`。
>
> 已知本文过时的几处，列出来是为了避免被误当成现状
> （下表括号里的路径都相对 `docs/design/`）：
>
> ```text
> 表结构    本文 §5.1 列了 23 张表名，现为 27 张（spec/03-数据模型.md）
>           身份模型已合并：bots + telegram_accounts → identities
>           事件两层已改名：inbound_events → canonical_events
>           outbox_events 已并入 delivery_tasks，不再是独立表
> 租户隔离  本文只提「RLS 作为第二道防线」一句；现在是四层防线 +
>           五条建表铁律 + 完整策略模板与实测陷阱（spec/05-安全与租户隔离.md）
> 工期      本文的「三阶段实施路线」没有工期数字；
>           现为 54 周瀑布计划（plan/00-里程碑与工期.md）
> 定位      本文未区分自用与 SaaS；现已定为多租户 SaaS（ADR-0001）
> ```
>
> 另有一份 `docs/技术评审归档-2026-09.md` 也已删除 —— 它的待决项全部收口，
> 结论已落进 `docs/design/decisions/`（尤其 ADR-0001 定位、ADR-0002 选 Rust）
> 与 `docs/design/spec/08-账号接入.md`（扫码登录自研）。

## 1. 产品定位

本平台是一个多租户 Telegram 内容监控与规则路由系统，支持两种接入方式：

1. **Bot API**：监控用户已授权给 Bot 的频道或群组。
2. **MTProto**：使用用户绑定的 Telegram 普通账号，监控该账号已加入的对话。

每个租户可配置多个 Bot、Telegram 账号、监控源、关键词、规则和分发目标。平台共享程序与基础设施，但凭据、规则、任务和日志必须按租户隔离。

### 1.1 非目标

第一版不实现：

- 控制 Bot 主动加入任意频道。
- 绕过 Telegram 权限或隐私限制。
- 默认监控用户账号的全部对话。
- 训练、微调或改进 AI/ML 模型。
- 跨租户共享私聊、私有群组或私有频道内容。

## 2. 核心原则

### 2.1 租户是第一级实体

`tenant_id` 表示数据所属的组织或工作空间，`user_id` 表示操作人。两者不应混用。

```text
Tenant
├── Members
├── Bots
├── Telegram Accounts
├── Sources
├── Rules
├── Targets
├── Delivery Tasks
└── Audit Logs
```

即使 MVP 暂时是“一个用户一个租户”，数据模型也保留 `tenant_members`，避免未来支持团队时重构业务表。

### 2.2 接收身份与发送身份必须明确

每个源都必须关联实际能访问它的 `receiver_identity_id`；每个目标都必须关联有发送权限的 `sender_identity_id`。

```text
Source Subscription
├── tenant_id
├── chat_id
└── receiver_identity_id

Target Subscription
├── tenant_id
├── chat_id
└── sender_identity_id
```

不能仅根据 `chat_id` 假设任意 Bot 或账号均有权读取或发送。

### 2.3 共享处理管道，不共享凭据边界

不同 Bot 和 MTProto 账号拥有独立的 Telegram 接收身份。平台可以共享 Webhook Gateway、标准化、队列、规则引擎和投递 Worker，但不能无条件地把多个用户的 Telegram Listener 合并为一个。

MVP 默认将原始事件按接收凭据和租户隔离。公开频道的跨租户去重属于后续优化，启用前必须明确授权、保留周期和可见性规则。

## 3. Telegram 能力边界

### 3.1 Bot API

- 用户负责创建 Bot，平台接收 Token 并调用 `getMe` 验证。
- Token 验证后立即加密，不得写入日志、错误追踪或明文备份。
- Bot 不能代替用户主动加入任意频道。用户必须将 Bot 添加到目标频道或群组并授予必要权限。
- 群组中实际可接收的消息受 Bot 权限和 Privacy Mode 影响。
- 向用户私聊投递前，用户必须先启动对应 Bot，并通过一次性绑定码建立平台用户与 Telegram `chat_id` 的关系。

BotFather + Token 是 MVP 的默认绑定方式。Telegram 已提供 Managed Bots 等更新的 MTProto 管理能力，但不列入第一版范围。

### 3.2 MTProto

- 每个 Telegram 账号使用独立 Session，不得在租户之间共享。
- Listener 是账号级长连接，不是每个频道一个独立连接。
- 用户选择启用的监控源，可以减少平台的规则计算和数据写入，但不保证减少 Telegram 向该账号发送的底层更新。
- 手机号、验证码和 2FA 密码只能为完成登录短暂处理，不得持久化。
- Session 是长期登录凭据，必须使用 KMS/Vault 管理的密钥进行信封加密。
- 任意时刻只允许一个 Worker 持有某个账号的有效运行租约。

## 4. 总体架构

```text
                           Telegram
                 ┌──────────┴──────────┐
                 ↓                     ↓
          Bot API Webhooks         MTProto Connections
                 ↓                     ↓
          Bot Gateway          MTProto Account Worker
                 └──────────┬──────────┘
                            ↓
                    Credential Ingress
                            ↓
                  Inbox / Event Deduplication
                            ↓
                    Canonical Event Store
                            ↓
                 Authorized Subscription Fanout
                            ↓
                       Tenant Events
                            ↓
                       Rule Engine
                            ↓
                 Delivery Task + Outbox
                            ↓
                      Delivery Queue
                            ↓
                      Sender Workers
                            ↓
                           Telegram
```

### 4.1 控制面

负责租户、成员、Bot、Telegram 账号、源、目标、规则、用量和审计管理。控制面不应直接持有已解密的 Token 或 Session。

### 4.2 Bot Gateway

所有 Bot 可共享同一个 FastAPI 服务，但每个 Bot 使用独立路由标识和 Webhook Secret。

```text
POST /telegram/webhooks/{bot_public_id}
X-Telegram-Bot-Api-Secret-Token: <secret>
```

`bot_public_id` 使用无业务含义的随机标识，用于路由而不是用于身份认证。真正的请求验证依赖 Telegram Webhook Secret Header，避免密钥出现在代理访问日志和 URL 历史中。

处理步骤：

1. 根据 `bot_public_id` 查找 Bot。
2. 常量时间比较 Webhook Secret。
3. 检查 Bot 和租户状态。
4. 使用 `(bot_id, update_id)` 完成 Inbox 去重。
5. 持久化后快速返回 `200`。
6. 后续规则与投递异步执行。

### 4.3 MTProto Account Worker

负责账号登录、Session 解密、长连接、更新监听、Dialogs 同步和断线重连。

水平扩展时，每个账号通过租约分配给唯一 Worker：

```text
account_id
worker_id
lease_expires_at
fencing_token
heartbeat_at
```

Worker 失联后由调度器重新分配。所有状态写入都携带 `fencing_token`，防止旧 Worker 恢复后继续写入。

## 5. 领域模型

### 5.1 核心数据表

```text
tenants
users
tenant_members

bots
telegram_accounts
credential_keys

telegram_chats
source_subscriptions
target_subscriptions

keywords
rules
rule_keywords
rule_sources
rule_conditions
rule_targets

inbound_events
tenant_events
media_objects

delivery_tasks
delivery_attempts
outbox_events

account_leases
audit_logs
```

### 5.2 凭据表

```text
bots
────
id
tenant_id
telegram_bot_id
username
token_ciphertext
key_id
webhook_secret_hash
status
created_by_user_id
created_at
updated_at

telegram_accounts
─────────────────
id
tenant_id
telegram_user_id
display_name
session_ciphertext
key_id
status
last_connected_at
created_by_user_id
created_at
updated_at
```

密文和密钥版本必须分离；数据库管理员不应仅凭数据库内容就能解密凭据。

### 5.3 源和目标

```text
telegram_chats
───────────────
id
platform_chat_id
chat_type
visibility_scope
owner_tenant_id
title
username
metadata_updated_at

source_subscriptions
────────────────────
id
tenant_id
chat_id
receiver_type
receiver_identity_id
enabled
created_by_user_id

target_subscriptions
────────────────────
id
tenant_id
chat_id
sender_type
sender_identity_id
permission_status
enabled
```

`visibility_scope` 取值为 `PUBLIC` 或 `TENANT_PRIVATE`。只有经确认的公开频道可作为全局元数据；私聊、私有群组和私有频道必须设置 `owner_tenant_id`。访问权、启用状态和收发凭据始终位于租户订阅表。

## 6. 事件和去重

### 6.1 两层事件模型

```text
CanonicalEvent
├── adapter
├── receiver_identity_id
├── source_chat_id
├── source_message_id
├── event_type
├── edit_version
├── text
├── media_metadata
├── occurred_at
└── received_at

TenantEvent
├── canonical_event_id
├── tenant_id
├── source_subscription_id
└── rule_context
```

`CanonicalEvent` 表示某个 Telegram 接收身份实际观测到的事件；`TenantEvent` 表示经过授权订阅后交给某个租户的处理单元。

### 6.2 幂等键

Bot API：

```text
(bot_id, update_id)
```

MTProto：

```text
(account_id, peer_id, message_id, event_type, edit_version)
```

投递：

```text
(tenant_id, rule_id, rule_version, tenant_event_id, target_subscription_id)
```

数据库中必须为上述幂等键建立唯一约束，不得仅依赖 Redis 短期锁。

### 6.3 事件类型

至少支持：

- `MESSAGE_CREATED`
- `MESSAGE_EDITED`
- `MESSAGE_DELETED`
- `MEDIA_GROUP_COMPLETED`
- `SOURCE_PERMISSION_CHANGED`

规则应明确是否在编辑、删除或补偿同步时重新执行。

## 7. 规则与投递

### 7.1 规则模型

```text
Rule
├── tenant_id
├── version
├── Sources
├── Keywords / Regex
├── Conditions
├── Media Types
├── Targets
└── Status
```

一条规则可关联多个源、关键词和目标。创建 `DeliveryTask` 时保存规则版本快照，避免用户修改规则后无法解释历史投递结果。

### 7.2 可靠投递

不直接在规则 Worker 中调用 Telegram 发送。使用事务 Outbox：

```text
PostgreSQL Transaction
├── INSERT delivery_task
└── INSERT outbox_event
              ↓
       Outbox Publisher
              ↓
        Redis Streams
              ↓
       Delivery Worker
```

`delivery_tasks` 至少包含：

```text
id
tenant_id
idempotency_key
sender_identity_id
target_subscription_id
status
attempt_count
next_retry_at
telegram_message_id
last_error_code
created_at
completed_at
```

投递 Worker 必须支持指数退避、最大重试次数、死信队列、按发送凭据限流和同一目标的有序发送。

## 8. 安全与租户隔离

### 8.1 权限模型

```text
SUPER_ADMIN
TENANT_OWNER
TENANT_ADMIN
MEMBER
VIEWER
```

`SUPER_ADMIN` 默认也不应看到明文 Token、Session 或消息正文。所有敏感操作均记录操作人、租户、对象、结果、IP 和时间。

### 8.2 数据库隔离

- 所有租户业务表包含 `tenant_id`。
- API 查询始终从已验证的成员关系推导 `tenant_id`，不信任请求体传入的租户标识。
- PostgreSQL Row Level Security 作为应用层授权之外的第二道防线。
- Worker 每次处理任务前重新验证租户、凭据和订阅状态。

### 8.3 凭据保护

- Bot Token 和 Session 使用每租户数据密钥加密。
- 数据密钥由 KMS/Vault 中的主密钥包装。
- 解密操作限制在 Bot Gateway、MTProto Worker 和 Sender Worker。
- 凭据不进入 Redis、通用队列或日志。
- 支持密钥轮换、凭据撤销、账号禁用和租户删除。

### 8.4 数据最小化

- 消息正文、媒体和原始 Update 分别配置保留周期。
- 默认不下载媒体，只在规则需要时延迟下载。
- 日志中不记录凭据、验证码、2FA 密码或完整消息正文。
- 租户删除后撤销 Webhook/账号会话，并对所有相关数据执行可审计清理。

## 9. 关键业务流程

### 9.1 绑定 Bot

```text
用户通过 BotFather 创建 Bot
  ↓
向平台提交 Token
  ↓
调用 getMe 验证
  ↓
加密 Token
  ↓
生成独立 Webhook 路由标识和 Secret
  ↓
调用 setWebhook
  ↓
删除内存中不再需要的明文 Token
```

### 9.2 添加 Bot 监控源

```text
用户将 Bot 添加到频道/群组
  ↓
平台检查身份和权限
  ↓
使用该 Bot 获取对话元数据
  ↓
创建关联 receiver_identity_id 的 Source Subscription
```

### 9.3 绑定 MTProto 账号

```text
提交手机号
  ↓
专用登录 Worker 发送验证码
  ↓
短暂接收验证码/2FA
  ↓
生成 Session
  ↓
信封加密并立即清除明文输入
  ↓
同步 Dialogs
  ↓
用户显式选择需要监控的源
```

### 9.4 投递给用户私聊

```text
用户启动自己的 Bot
  ↓
Bot 生成一次性绑定码
  ↓
用户在平台确认绑定
  ↓
保存 tenant_id + bot_id + private_chat_id
  ↓
创建 USER_SELF Target Subscription
```

## 10. MVP 部署

第一版保留四个运行单元：

```text
telegram-monitor/
├── app
│   ├── FastAPI Control Plane
│   ├── Bot Webhook Gateway
│   ├── Rule Worker
│   └── Delivery Worker
├── mtproto-worker
├── postgres
└── redis
```

推荐技术栈：

```text
Python 3.12+
FastAPI
aiogram
Telethon
SQLAlchemy 2.x
asyncpg
Pydantic
Alembic
PostgreSQL
Redis Streams
Caddy
Docker Compose
```

代码中保持模块边界，但不必在 MVP 就将每个模块拆成独立微服务。

## 11. 扩展策略

不按“用户达到 100 个”这类单一阈值拆分服务。根据实际指标决定：

- Webhook 请求率和 P95/P99 延迟。
- MTProto 活跃账号数与断线重连率。
- Inbox、Outbox 和 Delivery 队列深度。
- 规则匹配耗时和消息峰值。
- 数据库连接、写入延迟和热表大小。
- 单个凭据或目标的 Telegram 限流情况。

扩展顺序建议：

1. 独立 Delivery Worker。
2. 按账号分片 MTProto Worker。
3. 独立 Bot Gateway。
4. 独立 Rule Worker 并按 `tenant_id` 分区。
5. 对历史事件和日志进行分区或归档。

## 12. 可观测性与运维

至少监控：

- Bot Webhook 成功率、验证失败数和处理延迟。
- MTProto 连接状态、租约、重连次数和授权失效。
- 消息去重数、规则命中数和处理失败数。
- Outbox 积压、投递重试、死信数和 Telegram 限流时间。
- 凭据解密、密钥轮换和管理员敏感操作。

所有日志都应包含 `trace_id`、`tenant_id`、`component`、`event_id`，但不得包含凭据或完整消息正文。

## 13. 实施路线

### 阶段 1：安全的 Bot 闭环

- 租户、成员和权限模型。
- Bot Token 加密与 Webhook Secret 验证。
- Inbox 去重、标准化事件和源订阅。
- 关键词/正则规则。
- Outbox、幂等投递、重试和死信。

### 阶段 2：MTProto 账号

- 专用登录流程。
- Session 信封加密。
- 账号租约与 Fencing Token。
- Dialogs 同步与用户显式选源。
- 断线恢复和消息补偿。

### 阶段 3：完整事件生命周期

- 消息编辑、删除和媒体组。
- 发送权限定期复检。
- 租户配额、计费与滞后保护。
- 分区、归档和灾难恢复。

## 14. 上线前必须确定的决策

1. 一个租户是单用户还是支持多成员团队？
2. 是否允许监控私聊和私有群组？
3. 消息正文、媒体和投递日志的保留周期是多少？
4. 管理员能否查看租户内容，什么情况下允许？
5. 租户删除和凭据撤销的 SLA 是多少？
6. 是否允许跨租户共享公开频道的规范化事件？
7. 内容转发、版权投诉和滥用处理规则是什么？

## 15. 官方参考

- [Telegram Bot API](https://core.telegram.org/bots/api)
- [Telegram Bot Features](https://core.telegram.org/bots/features)
- [Telegram Bots FAQ](https://core.telegram.org/bots/faq)
- [Telegram API Terms of Service](https://core.telegram.org/api/terms)
- [Telethon Sessions](https://docs.telethon.dev/en/stable/concepts/sessions.html)
