# WP-5-delivery-payload-freeze：冻结重试投递载荷

## 任务与判据

任务 id：`WP-5-delivery-payload-freeze`。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-5 与 §三 G4；投递任务定义见 `docs/design/spec/03-数据模型.md` §六；迁移与权限纪律见 `docs/design/eng/02-本地环境与迁移.md`、`docs/design/spec/05-安全与租户隔离.md`。

当前 `delivery_tasks.telegram_random_id` 固定后，重试仍通过关联 `tenant_events`、`canonical_events`、`source_subscriptions` 和 `rule_versions` 重建发送载荷。若其中影响投递的值发生变化，同一任务可能以原 `random_id` 发送不同内容或路由。本提案请求批准为新任务持久化不可变的投递输入快照，并在每次尝试时只从该快照构造载荷。

## 建议方案

- 批准记录：用户已在本会话批准实施本提案，并于 2026-10-15 允许扩展到必要的审计定义与门禁反证。新增列沿用现有表的 RLS 自动发现规则；扩展的 `C6` 专门检查投递不可变列权限，不改 RLS 策略。
- 新增迁移及 `migrations-test/` 镜像，为 `delivery_tasks` 增加 JSONB 投递快照列。新任务在入队事务中保存构造投递消息所需的完整输入：发送身份、目标 peer、动作、来源 chat/message、媒体与文件标识、渲染后的消息文本及解析模式。快照不得依赖后续读取可变的来源订阅、事件或规则记录。
- 快照与 `telegram_random_id` 在同一事务写入。二者一经分配均不可由普通应用角色改写；发送、失败、回收等合法状态更新仍按现有最小列权限工作。
- 投递读取路径从任务快照构造 `DeliveryPayload`，不再联查可变业务记录来决定请求参数。修改规则、来源资料或目标配置只影响新建任务；已创建任务保持原快照。需要改变既有任务内容时创建新任务，不复用原 `random_id`。
- 对已有任务不从当前关联数据静默回填快照。缺少稳定 ID 或快照的历史任务继续隔离，不进入自动发送；发布前按 WP-5 random ID 提案中的兼容流程暂停 worker 并对账。
- 快照仅包含发送所需字段，不复制不参与渲染或传输的事件原始载荷、密钥或凭据。若实现发现必须保存额外字段，先更新本提案并明确其数据保留影响。

## 需要改动

- 新增幂等迁移和匹配的测试迁移；不修改已应用迁移，不变更 RLS 策略。快照列带 `tenant_id` 的任务表现有 RLS 边界不变。按后续授权补 `C6`、门禁定义和双遍反证，检查运行时角色不能更新投递不可变列。
- 更新 `crates/db/src/repo.rs` 入队与读取路径及投递相关记录类型；更新 `crates/app/src/delivery.rs`、`crates/app/src/pipeline.rs`，使完整发送输入在任务首次创建时固化，重试和进程重启后只读取固化值。
- 更新 DB 与 pipeline 测试，覆盖普通角色不能改写快照、重试载荷稳定、来源/规则/目标后续变更不影响旧任务、幂等重复入队不覆盖原快照，以及状态迁移权限仍可用。
- 更新 `docs/design/spec/03-数据模型.md` 与 `docs/plan/evidence/`，记录快照字段、历史任务处置和验证结果。
- `Cargo.lock` 中 `rustls 0.23.45` 的 RustSec 修复属于独立依赖安全工作，不纳入本提案，也不得通过放宽 `deny` 处理。

## 安全与兼容边界

- `telegram_random_id` 与快照必须作为一个不可拆分的不变量：同一任务每次尝试使用同一账号、peer、动作、来源和消息内容。网络结果不确定时不得更换 ID 或重建快照。
- 新增列不改变历史数据的发送资格。历史任务没有可证明的首次请求内容，不能仅凭当前关联行推断其此前尝试所用的载荷。
- 数据库 owner 可修改迁移、触发器或数据；约束针对普通应用角色和正常写路径，不宣称抵御 owner 恶意操作。
- `MemorySession` 下的 Telegram 更新恢复能力、跨租户或跨数据库共享发送身份、随机 ID 的全局协调仍按 `WP-5-delivery-random-id` 的边界处理；快照本身不证明 Telegram 服务端去重行为。

## 验收

- 本地实施与完整门禁结果见 `docs/plan/evidence/WP-5-投递请求快照与重启-2026-10-15.md`。稳定 ID、请求快照、历史任务隔离与模拟 transport 子进程恢复已验；真实 Telegram 与 G4 仍未验。本提案末尾 report 只描述获批实现的本地交付，不代表工作包或关口通过。
- 迁移镜像一致，`scripts/migrate-idempotent.sh` 通过；受限角色测试验证快照不可变且合法任务状态更新可用；SQLx 缓存、RLS 审计与审计双遍反证按 `docs/design/eng/04-CI门禁.md` 执行。
- DB 与 app 测试证明首次发送、普通重试、租约回收和进程重启使用字节等价的发送参数；更改规则、来源订阅展示信息或关联事件内容后，旧任务仍从快照生成相同请求。
- 迁移前已存在的未完成任务逐项盘点；无法证明其请求内容和发送状态的任务不自动恢复发送。处置记录写入 `docs/plan/evidence/`。
- 完整本地门禁通过后更新对应 PR 并等待 CI 裁决。真实 Telegram 账号测试和 G4 证据仍单独验收，不由本提案或模拟测试代替。

## 升级边界

本提案涉及新迁移、任务表写入约束和投递安全不变量，命中根 `AGENTS.md` §五 升级条件。获批前不实施；获批后不修改已应用迁移、`scripts/`、`.github/`、`crates/db/src/audit/` 或 backlog，除非相关改动另行批准。

[report]
task: WP-5-delivery-payload-freeze
path: ok
gates: pass
escalation: docs/plan/proposals/WP-5-delivery-payload-freeze.md
