# WP-5-delivery-restart：投递进程重启语义

## 任务与判据

任务 id：`WP-5-delivery-restart`。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-5 要求后台任务在进程重启后继续；同文 §三 G4 另要求进程强杀后不丢事件、不重复投递。投递幂等键的数据库约束见 `docs/design/spec/03-数据模型.md` §5.2。

本提案只请求确定投递外部副作用的恢复语义。在语义获批、基线变更完成前，不新增 Delivery 进程测试，不改生产 worker、数据库结构、迁移、CI 或基线判据。

## 已确认的实现边界

`crates/app/src/delivery.rs` 的 `run_delivery_once` 先回收陈旧 `SENDING` 任务，再领取任务并调用 `DeliveryTransport::deliver`，成功后才将任务写为 `DONE`。因此，外部平台已接受消息、但进程在 `DONE` 提交前被强杀时，重启会回收并再次调用 transport。

`delivery_tasks` 的幂等键阻止同一逻辑任务重复入库，不会撤销已经发生的外部发送。使用测试 transport 可以验证任务状态恢复，却不能证明真实 Telegram 不会收到重复消息；不接入真实账号也不能覆盖平台接受请求与本地提交之间的崩溃窗口。

协议核对（2026-10-13）：Telegram MTProto 的 `messages.sendMessage` 与 `messages.forwardMessages` 都带 `random_id`。Telegram 文档说明，同一账号向同一 peer 重用已使用的 ID 时，服务端返回此前生成的消息而不再次发送；已用 ID 不过期，且规则跨账号 session 与请求方法类型生效。若原请求仍在服务端处理中，重复请求会返回 `RANDOM_ID_DUPLICATE`。文档还说明 `updateMessageID` 可将 `random_id` 与生成的消息 ID 对应，并能通过更新恢复流程再次送达。见 [Telegram Updates：random_id 去重与恢复说明](https://core.telegram.org/api/updates)、[messages.sendMessage](https://core.telegram.org/method/messages.sendMessage) 与 [messages.forwardMessages](https://core.telegram.org/method/messages.forwardMessages)。

仓库锁定的 `grammers-client 0.10.0` 高层 `send_message` 和 `forward_messages` 每次在库内生成 `random_id`，没有公开传入稳定 ID 的参数；`Client::invoke` 与重新导出的 TL 类型则可供 `tgm-tg` crate 使用。仓库的 raw invoke 边界本来就收敛在 `crates/tg/`。因此候选路径是：在请求发出前为 outbox 任务持久化稳定 ID，COPY/RESEND 与 FORWARD 的重试都复用它，并在 `tgm-tg` 用 raw invoke 传递；不能在每次尝试时新生成 ID。

当前 `delivery_tasks` 没有持久化 Telegram `random_id`，现有 `idem_key` 是文本哈希，不能直接作为 MTProto `long` 使用。稳定 ID 需要在首次请求前持久化，并按 Telegram 文档规定的账号与目标 peer 去重范围保证不冲突；数据库如何表达此字段及其唯一性要另行设计。还需处理 `RANDOM_ID_DUPLICATE` 在途响应，以及请求成功后响应丢失时由更新流恢复消息 ID 的路径。协议文档描述了服务端行为，但本地测试不能证明真实账号、所有投递动作及当前锁定库组合下的完整恢复结果。

## 需要维护人决定

建议优先批准并验证选项 3，以 MTProto 稳定 `random_id` 争取保留现有目标语义；若实现或 Telegram 行为验证不成立，再由维护人决定是否调整 G4 判据：

1. **至少一次**：任务不因崩溃丢失，允许外部平台在不确定窗口收到重复消息；产品与对客说明需明确这一边界。
2. **至多一次**：避免自动重复发送，但进程在发送与完成记录之间崩溃时可能永久丢失消息，不符合当前“不丢事件、不重复投递”两项同时成立的字面要求。
3. **外部幂等或对账（建议先验证）**：批准后为每个投递任务持久化一个在同一 Telegram 账号和目标 peer 上跨重试稳定、且不与其他发送冲突的 `random_id`；在 `tgm-tg` 使用 raw invoke 传入它，并处理重复请求响应到本地消息记录的映射。设计时需确定新增状态的存储与唯一性约束、ID 在首次请求前落库、旧任务兼容、发送身份复用范围、`RANDOM_ID_DUPLICATE` 在途响应与 `updateMessageID` 恢复路径，以及 COPY/RESEND 和 FORWARD 两类调用。若协议或实现验证不足，再评估对账方案；不以本地 `idem_key` 单独替代 Telegram 去重。

若要求同时保证不丢失和不重复，须先证明存在可执行的外部幂等/对账方案。若该要求不可实现，维护人需明确选择可接受的语义，并通过正式变更流程修订相应基线；执行器不自行降级判据或新增 ADR。

## 获批后的验收范围

- 若批准稳定 `random_id` 路径，需先提交包含迁移、测试迁移与数据库唯一性设计的实施提案，按 §五 获批后再实现；进程故障测试应覆盖请求前终止、平台接受后本地 `DONE` 前终止，以及同一 ID 重试后的本地收敛。
- 本地断言可核验 ID 分配、持久化和重试复用；受控 Telegram 测试账号还需验证同 ID 重放返回既有消息、在途重复请求的处理，以及两类调用在进程重启后的结果。测试 transport 不能代替这些平台证据。
- 任何新增迁移、权限、审计、门禁或冻结文档改动仍分别遵守根 `AGENTS.md` §五 的升级纪律。
- 本任务结果不单独证明 WP-5 整体或 G4 通过。

[report]
task: WP-5-delivery-restart
path: escalated
gates: not-run
escalation: docs/plan/proposals/WP-5-delivery-restart.md
