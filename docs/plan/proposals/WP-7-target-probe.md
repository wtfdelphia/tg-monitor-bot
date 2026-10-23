# WP-7：目标频道连通性自检

任务 id：`WP-7-target-probe`。

状态：2026-10-23 维护人批准本提案，开始实施。真实 Telegram 验收尚未执行，没有授权提交、推送或开 PR。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-7，
`docs/design/spec/07-接口契约.md` §3.8、§4.2，
`docs/design/decisions/ADR-0019-目标频道连通性自检.md`。
安全与架构边界：`spec/02` §1.1、§3.1，`spec/05` §七，ADR-0010、ADR-0032。

## 一、现状与复用

基线为 PR #45 合入 `dev` 的 `ad3e265803fa0ee15aa26ba7c169fb6c5e53afbe`。
目标页面已有列表、创建、编辑和停用，但没有自检按钮；Router 与 OpenAPI 没有目标 probe 端点。
`target_channels` 只有 `can_send`、`checked_at` 两个探测字段，创建后均为 `NULL`，没有后台探测。

复用现有目标行、租户事务、Owner/Admin 写权限、Cookie/CSRF、页面提交锁、delivery 进程生命周期、
`load_bot_credentials` 与 `TelegramBotApiClient`。没有能直接承担目标探测的现成任务状态机。
不把探测塞入 `delivery_tasks`：它的不可变投递快照、`random_id`、消息发送与日志语义不适用于只读探测。

此前控制台二维码改动仍独立保留在 `wp7-console-qr` 工作目录。本提案使用单独的干净工作目录，不把两片源码混写，也不把上一片门禁结果当作本片通过。

## 二、需要批准的决定

### 2.1 后台执行，不给控制面解密权限

建议采用“PostgreSQL 持久待办 + delivery 角色执行 + 页面读取结果”。
control-plane 只在目标行写入待探测状态，仍只有 `kms:Encrypt`；delivery 按当前租户和发送身份加载凭据后调用 Telegram。
组件间没有内部 HTTP/RPC，Redis 不承担唯一任务状态，也不承载秘密。

ADR-0019 的“结果当场返回”与后台执行存在明确差异。本提案申请在实施时新增 ADR，取代其中的同步返回要求：
创建即持久化排队，页面随即显示检查中，完成后显示结果。首发必须有自检的决定保持不变；worker 不在线或 Telegram 未应答时不得显示通过。
原 ADR 不原地改写，只追加被取代的定位。

没有选择给 control-plane 增加 Decrypt，也没有选择新增探测服务、RPC、凭据重输表单或测试消息。
HTTP 请求内等待 worker 仍可能超时，不能保证同步结果，却增加长请求与重复创建处理；本片不采用。

### 2.2 明确目标属性，不伪造来源转发能力

官方 Bot API 的 `getChat` 返回指定聊天的 `ChatFullInfo`；`has_protected_content` 表示该聊天的消息不能转发出去，
`getChatMember` 返回指定成员状态，频道管理员的发帖权使用 `can_post_messages`。
`forwardMessage` 还受原消息内容保护限制。因此，目标 Bot 能发言不等于它能转发任意来源的消息。

申请修订 `spec/07` §3.8 中尚未实现的响应定义：

- `can_send` 表示所查目标此时的基础发言权限，不保证所有媒体、话题或来源动作均成功。
- `bot_role` 是所查发送 Bot 的成员角色；必须核对返回的 chat id、user id 与任务绑定一致。
- `content_protected` 明确表示所查目标聊天的内容保护，不是来源频道保护；不把该字段单独作为禁止向目标发送的依据。
- 删除未定义来源上下文的 `can_forward`，不把它设成 `can_send` 的别名。来源消息能否 FORWARD 继续由既有真实投递路径判断，不自动改为 RESEND，不扩展为来源权限探测。

官方核对来源（2026-10-23）：`https://core.telegram.org/bots/api` 的 `getChat`、`ChatFullInfo`、`getChatMember`、
`ChatMemberAdministrator`、`ChatMemberRestricted`、`ChatPermissions`、`forwardMessage`、`ResponseParameters` 章节。
方法文档对其他用户的成员查询保证不能直接套用到 Bot 自身，仍需测试 Bot 核对实际响应。

## 三、实施范围

### 3.1 持久状态和迁移

扩展 `target_channels`，不新增表。发送身份的跨目标冷却保存于 `identities.target_probe_not_before`，仅追加该列的 UPDATE 授权，不扩大既有列权限；只增迁移，新增状态/代次、尝试次数、下次执行时间、认领令牌/到期时间、稳定错误码及
`content_protected`、`bot_role`；复用 `can_send`、`checked_at`。
拟定状态为 `PENDING`、`RUNNING`、`SUCCEEDED`、`FAILED`；成功完成查询与“可发言”分别表达，查询明确得出 `can_send=false` 不能显示绿色可发送，查询失败时该值保持未知。

历史目标清空旧探测值后进入待探测状态。新建目标、实际更换 chat/发送身份以及显式重试，在事务内登记待办并清空旧结果；只改标题或启停保留结果。
对同一代次已排队/在途的重复请求复用状态，不创建重复任务。新增列沿用现有 RLS，不改 RLS 策略、数据库角色或 KMS 权限。

worker 通过受限租户事务原子认领到期目标，提交认领后再请求 Telegram，不能持数据库锁等待网络。
回写同时校验租约令牌、租约有效期、探测代次、目标路由及凭据版本；旧响应不得覆盖新结果。
崩溃后租约到期恢复待办，停用目标的显式自检也能执行。

凭据替换和身份失效路径须同事务使相关目标结果失效；更新前已取出的旧凭据不能在更新后写回有效探测结果。
复用现有凭据写回的版本比较形状，不新增明文、凭据副本或解密缓存。

### 3.2 delivery 执行与错误边界

在现有 delivery 进程内增加独立、有界的探测调度，随进程退出信号停止；不依赖 MTProto 登录完成才能检查，也不让探测失败或限流阻塞既有投递循环。
每次执行按任务绑定的租户和 Bot 加载当前凭据，只查询自身成员状态；缺凭据、非 Bot、失效/封禁身份在解密或网络请求前拒绝。

复用现有 Bot API 客户端的禁止重定向、请求超时和响应大小上限，增加类型化 `getChat` / `getChatMember` 与 `retry_after` 解析。
不向外回显或记录原始 description、Token、完整请求 URL 或响应体；只保存稳定错误码，不能把 Telegram 无法区分的情况编造成精确失败原因。

权限按服务端实际返回的聊天类型与成员类型判断，不能仅因 `getChat` 成功就判定是成员或能发言。
频道按成员角色及 `can_post_messages` 判定；不能把频道专用字段套到群组。
群组普通成员结合默认 `permissions` 判断，受限成员结合 `is_member` 与自身 `can_send_messages` 判断；管理员不套用普通成员限制。
已退出或被踢成员不可发言；判定必需字段缺失、未知状态、身份不匹配和不支持的聊天类型按未知/失败处理。
先覆盖频道与现有需求支持的群组类型，其他类型须明确拒绝，不静默推断。
全程不发测试消息、不改变成员权限和投递动作。

每发送 Bot 串行且全局并发有上限；429 按服务端 `retry_after` 冷却，手工重试不能提前绕过同一 Bot 的冷却。
网络错误/5xx 有界退避；权限不足等确定拒绝不自动高频重试。冷却与重试状态持久化，重启不能清掉 Telegram 等待时间。
页面刷新只读状态，不触发 Telegram 探测。

### 3.3 HTTP 和控制台

拟定接口为：

```text
POST /api/v1/targets
    → 201：目标和待探测状态在同一事务创建
POST /api/v1/targets/{id}/probe
    → 202：请求重试或复用现有在途状态
GET /api/v1/targets/{id}/probe
    → 200：当前代次、状态、can_send、content_protected、bot_role、checked_at、error_code
```

Owner/Admin 可以触发探测；GET 沿用租户目标可见性，只读角色可看结果但不能触发。
跨租户目标统一 404，租户来自认证上下文，不采信 `tenant_id` 参数。
未探测或失效的结果字段为 `null`，返回 `no-store`；角色拒绝、CSRF 与既有错误包络保持一致。
公开 OpenAPI、目标列表响应、spec/03、spec/07、架构交接及部署说明同步更新，不修改 CI 卡口。

目标页显示基础发言权限、Bot 角色、目标内容保护、检查时间及稳定错误提示。
创建后立即显示待检查，每 2 秒串行读取状态，单次页面自动等待最多 60 秒；超时仅停止页面等待，后台持久待办继续执行，文案不能宣称后台任务已取消。
终态、离页、刷新或重新开始时停止旧轮询、拒绝迟到响应。
同源凭据、CSRF、`cache: no-store`、现有提交锁和只读角色限制全部保留；手工重试节流与服务器一致。

## 四、验收与边界

- 数据层：迁移可重入、历史目标初始化、状态约束、受限角色租户隔离、并发认领、过期租约、路由 A→B→A 和凭据替换后迟到结果拒写；标题/启停更新不得误清结果。
- 调度：worker 被杀后从 PG 恢复；同一 Bot 冷却跨目标和重启仍有效；探测不阻塞正常投递；缺少/失效凭据不留下永远 RUNNING 的任务。网络和 429 使用本地桩，不为验收故意冲击 Telegram 限流。
- 安全与接口：Owner/Admin、只读角色、跨租户 404、CSRF、状态轮询、no-store、原始错误和秘密不外泄；control-plane 无 Decrypt、无凭据读取调用；对应 R14、R15、R16 形状仍通过。
- 页面：Node 执行实际页面脚本并做旧响应/重复提交反证；真实 Chrome 经受限 HTTP 与本地 Bot API 桩验证创建、重试、失败恢复、窄屏、键盘和请求审计。本地桩不记作 Telegram 真机验收。
- 真机：实施后另约维护人使用本地权限受限文件/环境提供测试 Bot 凭据和测试频道；核对可发言、无发帖权、Bot 不在频道、目标内容保护切换等只读结果。改权限或撤销测试 Token 由维护人明确操作；不复用历史聊天 Token，不在对话或证据粘贴凭据，不自动变更线上 Bot 权限。
- 最终执行完整 `scripts/ai-driver/ai-gates.sh`，包括迁移幂等、RLS 审计与 double-pass，并显式运行浏览器验收。CI 仍是最终裁决；没有真机证据或维护人出口确认，不宣布 WP-7 通过。

## 五、批准范围与当前结果

维护人已批准以上后台执行设计、同步语义修订的新 ADR、迁移与持久状态、凭据/身份失效联动、Bot API 只读调用、权限和字段语义、公开接口及页面、自动化测试与测试 Bot 真机验收范围。
已实施数据库、后台、接口和页面；首轮完整门禁在 C2 反证恢复清单处失败，后续修复见
`WP-7-target-probe-audit-restore.md`。修复后的完整本地门禁和显式 Chrome 验收均通过。
2026-10-23 已完成真实 Telegram 正向只读自检，以及第二个测试 Bot 的拒绝路径验收；
记录见 `docs/plan/evidence/WP-7-目标频道自检-2026-10-23.md`。无发帖权与内容保护切换场景仍待维护人准备专用测试状态，
并由维护人明确变更权限；本提案不授权自动修改 Bot/频道权限。
不包含新增依赖、表、IAM/KMS 权限、组件 RPC、发送测试消息、提交发布或宣布 WP-7 通过。

已运行定向测试、页面脚本、真实 Chrome 和修复后的完整门禁，详细结果与未覆盖项登记在本片证据中；
不把本地 Bot API 桩记为 Telegram 真机，不复用历史 Token，也不宣布 WP-7 通过。
构建结束清理本片专属 target 和有归属的临时资源，保留源码、`.sqlx`、证据、日志和正在使用的 Compose 栈/具名卷。

[report]
task: WP-7-target-probe
path: escalated
gates: pass
escalation: ->
