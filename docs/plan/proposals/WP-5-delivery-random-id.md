# WP-5-delivery-random-id：稳定 Telegram 请求标识

## 任务与判据

任务 id：`WP-5-delivery-random-id`。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-5 与 §三 G4；投递任务唯一键见 `docs/design/spec/03-数据模型.md` §5.2；数据库角色边界见 `docs/design/spec/05-安全与租户隔离.md` 与 `docs/design/eng/02-本地环境与迁移.md`。

用户已批准优先验证 MTProto 稳定 `random_id` 方向。本提案请求批准具体数据库安全改动及测试范围；在获批前不改迁移、RLS/GRANT、审计定义或生产发送链路。

## 协议事实与方案

2026-10-14 核对 Telegram 官方 MTProto 文档：[`messages.sendMessage`](https://core.telegram.org/method/messages.sendMessage) 与 [`messages.forwardMessages`](https://core.telegram.org/method/messages.forwardMessages) 均接受 `random_id`。同一账号向同一 peer 重用已用 ID 时，服务端返回之前生成的消息而不再发送；记录不失效，去重跨 session 与请求方法类型。若前一个请求仍在服务端处理，会返回 `RANDOM_ID_DUPLICATE`。生成消息的调用会产生 `updateMessageID`，其中包含 `random_id` 与消息 ID；即使方法响应丢失，该 update 也会通过更新恢复流程（例如 `updates.getDifference`）重新送达。出处：[Telegram Updates](https://core.telegram.org/api/updates) § `updateMessageID` updates。

不直接复用 `delivery_tasks.id`。该主键虽在正常单库运行期间递增，但它不是随机请求标识；备份恢复可能回退序列，旧任务或其他发送端也可能已在同一账号与 peer 使用相同数值。Telegram 的去重域不包含本项目任务，因此碰撞可能把新任务误映射为旧消息。

为每个新投递任务持久化独立的 Telegram `random_id`，在首次网络请求前生成并提交；重试和进程重启只读取该值，不得重新生成。采用 64 位非零随机值，并在当前数据库可见的 `(tenant_id, sender_identity_id, target_tg_chat_id)` 范围内建立部分唯一约束；唯一索引包含 `tenant_id`，符合本仓租户侧信道纪律。若同一 Telegram 账号会被多个租户或独立数据库同时用于发送，数据库约束不能跨越这些边界；上线前必须明确账号独占/协调策略，并将随机碰撞残余风险记录为运行边界，不能声称全局数学唯一。

`tgm-app` 将持久化的请求 ID 随任务传入 transport，`tgm-tg` 在 `crates/tg/` 内通过锁定的 `grammers-client 0.10.0` raw `Client::invoke` 构造 `messages.sendMessage` 或 `messages.forwardMessages`。COPY/RESEND 与 FORWARD 的重试均复用任务 ID。raw invoke 不越出 `tgm-tg`，符合 ADR-0020。

按响应中的 `updateMessageID.random_id` 精确关联 Telegram 消息 ID，再沿现有 `complete_delivery_task` 写入 `telegram_message_id`。不得仅凭响应中任意消息或本地任务状态猜测成功。若收到 `RANDOM_ID_DUPLICATE`，先处理响应中可关联的映射；无映射时保留原 ID 并进入可恢复状态，不换 ID 重发。现有投递失败策略有有限重试上限，实施时必须为此错误定义不会把不确定的已发送任务静默转成终态的恢复路径；单纯将其标为普通 Retryable 不足以满足 G4。

## 迁移与兼容

- 新增 `telegram_random_id BIGINT` 字段；历史行保持 `NULL`，不得用任务主键回填，也不得在 worker 重试时临时生成。
- 新建任务必须在首次发送前持久化非零 ID。以带 `tenant_id` 的部分唯一索引防止当前数据库、同租户、同发送身份和目标 peer 内重复分配。
- 发布前暂停旧 worker，并盘点所有尚未终结的旧任务。旧版本未持久化其历史 `random_id`，任何已发送状态不确定的旧任务必须隔离并人工对账；不能自动套用新 ID 重试。确认从未发出请求的旧任务，才可按批准的迁移流程分配新 ID 后继续。
- 不修改已应用迁移；新增迁移与 `migrations-test/` 镜像同步。更新 `docs/design/spec/03-数据模型.md` 的投递任务定义，并在 `docs/plan/evidence/` 记录兼容处理与验证范围。

## 需要改动

- 新增幂等迁移及测试迁移：`telegram_random_id`、带 `tenant_id` 的部分唯一索引，以及防止已分配请求 ID 被普通应用角色改写的约束；迁移不新增表、不改既有迁移。数据库 owner 可更改 DDL 或绕过触发器，故该机制不宣称抵御 owner 恶意操作。
- `crates/db/src/repo.rs` 与相关测试：任务创建时持久化非零随机 ID；历史 NULL 任务不能被领取发送；以受限角色验证 ID 不可改、唯一约束生效及合法任务状态更新仍可用。
- `crates/app/src/delivery.rs` 与 `crates/app/tests/pipeline.rs`：将同一 ID 贯穿首次发送、普通重试、回收和进程重启；覆盖 `RANDOM_ID_DUPLICATE` 不被普通有限重试策略错误终结。
- `crates/tg/src/client.rs` 与测试：raw 请求参数使用传入 ID；验证 `updateMessageID` 精确映射及缺失映射的错误路径。发送身份、目标 peer、动作和源消息必须在同一任务重试间保持不变；若现有数据模型不能保证，实施前补足其不可变边界。
- 更新投递数据模型与证据文档；SQL 变更同步更新 SQLx 缓存及审计 SQL/门禁所需的实现描述，不扩大到无关规则。

## 不变量与风险边界

- 首次请求前已提交的 `telegram_random_id` 是一个投递任务的唯一请求 ID；不确定结果只能重试同一 ID，不能以新 ID“恢复”。
- 同一 ID 只对相同 Telegram 账号、目标 peer 与请求内容有意义。任务的发送身份、目标、动作和源消息必须保持稳定；更改目标或内容应创建新任务，而不是改写已尝试任务。
- 租户级唯一索引无法协调共享同一 Telegram 账号的不同租户、其他服务或独立数据库。部署必须限制共享发送身份，或另行设计经批准的跨租户协调；不得把局部唯一约束描述为 Telegram 去重域的完整唯一性证明。
- `tenant_events` 级联删除可能删除任务行。随机碰撞概率很低但非零，删除数据库记录也不能撤销 Telegram 保存的已用 ID；数据恢复、导入和账号迁移流程须避免将历史 ID 重新分配。若无法满足，应暂停自动发送并人工对账。
- 当前 `grammers` 使用 `MemorySession`。稳定请求 ID 不等于会话状态持久化；若需在响应丢失后依赖 `updates.getDifference` 恢复映射，必须先证明当前客户端能持久化并恢复 Telegram 更新状态，否则 G4 此项仍未验。

## 验收

- `scripts/migrate-idempotent.sh` 证明新增迁移可重入、测试镜像一致；不修改已应用迁移。
- 受限角色测试证明已分配 ID 不可改、租户范围内的重复分配失败、历史 NULL 任务不发送、合法状态更新不受影响；RLS 审计、审计双遍反证、SQLx 缓存与完整本地卡口通过。
- transport 回归证明首次调用、普通重试、进程重启后的调用均使用同一 ID；成功映射消息 ID 后数据库状态正确；丢失映射与 `RANDOM_ID_DUPLICATE` 不会导致换 ID 或错误终态。
- 子进程故障测试覆盖请求前终止、请求已发出而本地 `DONE` 未提交、响应丢失及重启恢复。模拟 transport 仅证明本地状态，不代表 Telegram 服务端行为。
- 受控 Telegram 测试账号验证同 ID 重放、跨方法同 peer 去重、在途 `RANDOM_ID_DUPLICATE` 与更新恢复；验证期间需确认没有其他发送端共享测试账号。无真实账号证据时，外部行为保持未验。
- G4 需另以可核验的真实目标消息证明强杀后无重复且最终有本地完成记录；本提案、单测与 CI 均不替代该证据，也不宣布 WP-5/G4 通过。

## 升级边界

本提案涉及新增迁移、数据库约束和投递安全不变量，命中根 `AGENTS.md` §五 的升级条件。获批后仍须遵守 SQL、权限与审计的高风险验证；未获批前不实现。不得改动 `scripts/`、`.github/`、`crates/db/src/audit/` 或 backlog，除非相应改动另行获批。

本地通过和远端 CI 均不自动宣布 WP-5 或 G4 通过；旧任务对账、共享账号边界、灾难恢复风险与真实 Telegram 外部证据分别记录。

[report]
task: WP-5-delivery-random-id
path: escalated
gates: pass
escalation: docs/plan/proposals/WP-5-delivery-random-id.md
