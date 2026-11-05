# G4：L3 通知指纹去重实施提案

任务 id：`G4-L3-notify-dedup`。

状态：已批准并完成代码实施（2026-11-05）。实际实施基线为 PR #92、#93 合入后的
`cloud/dev` `42a2fd6`；静态门禁、可信数据库门禁和本片定向测试已通过；真实 Telegram
外部验收仍未运行。

判据出处：`docs/design/plan/00-里程碑与工期.md` §4.3 WP-3、§三 G3/G4；`docs/design/spec/03-数据模型.md` §5.3；`docs/design/spec/04-数据生命周期与容量.md` §一；`docs/design/eng/03-测试策略.md` L1/L2/L3；`docs/design/eng/04-CI门禁.md` 的审计与双遍要求。

## 一、当前缺口

当前仓库已有 `notify_dedup` 表、主键和到期清理，但 `cloud/dev` 没有真正的业务写入、判定或重算入口。现有投递幂等键解决的是同一租户、同一规则版本、同一事件与目标的重复入队，不等价于 L3 内容指纹去重；只补证据文件不能证明 WP-3 或 G4。

因此本片不把已有表结构误记为已完成，先请求批准 L3 的完整运行语义，再进入迁移、repo、pipeline、delivery 和测试实现。

## 二、拟定语义

### 2.1 指纹输入

指纹必须能从保留期内的 `canonical_events` 重算，并且同一条内容在不同规则处理顺序下得到相同结果。建议固定为版本化的 UTF-8 规范串：

```text
tgm:l3:v1\0<sender_id 或空串>\0<排序去重后的命中关键词 JSON>\0<正文前 300 个 Unicode 标量值>
```

对该规范串计算 SHA-256，保存为小写十六进制 `fingerprint`。具体规则如下：

- `sender_id` 缺失时使用空串；不把租户、目标频道或事件 id 放入指纹，这些字段由主键分隔；
- 命中关键词按 Unicode 字节序排序并去重，再编码为 JSON 数组，避免规则扫描顺序改变指纹；
- 正文优先取 `text`，为空时取 `caption`，都为空时使用空串；按 Unicode 标量值截取前 300 项，不按字节截断；
- 规范串带 `v1` 前缀，未来改变规范时生成新版本，不能静默重算旧值；
- 脱敏后 `sender_id`、`text`、`caption` 或 `content_hash` 被清空的事件不声称仍可复算，按 `spec/04` 的既定隐私边界记录为不可复算样本。

实施前需核对现有对客/内部文档是否接受上述规范；若必须保持注释中的 `sender|hits|text[:300]` 字面格式，应将分隔、排序和截取规则写成同样明确的版本化规范，不能直接用未转义字符串拼接。

### 2.2 原子占位与发送确认

建议将现有 `notify_dedup` 从“仅有已发送时间的记录”扩展为两阶段状态：

- `CLAIMED`：与新建 `delivery_tasks` 在同一事务中产生，绑定 `task_id` 和 `claimed_at`；同一 `(tenant_id, fingerprint, target_ref)` 窗口内已有有效占位时，不再新建投递任务；
- `SENT`：外部发送成功并写入 `telegram_message_id` 后，在完成任务的同一数据库事务中确认，记录 `last_sent_at` 和新的 `expires_at`；
- 可重试失败：释放本次占位或延长到明确的短暂 claim 超时，不能把失败任务永久当作已发送；
- 永久失败/人工终止：清理占位并保留任务的 DEAD 记录，不能阻塞未来同内容的新投递；
- 进程在外部发送后、数据库确认前被强杀：保留 claim，重启回收任务时沿用同一 `random_id`，先按已有真实消息映射/平台响应路径收敛；在无法证明已发送前不得擅自创建第二个 L3 通知。

占位必须由数据库唯一键和行锁裁决，不能采用“先 SELECT 再 INSERT”的检查后写入形状。任务创建、占位创建和事件处理状态推进必须处于同一事务；任一失败都回滚，不能留下没有任务的 claim。

### 2.3 兼容与留存

- 已有 `notify_dedup` 行没有 `task_id` 或状态信息时，迁移不得猜测其对应任务；按保守规则转换为 `SENT`，保留原 `last_sent_at` 与 `expires_at`；无法证明时间有效的行不得被提前删除；
- 现有 `notify_dedup` 的按 `expires_at` 每小时清理继续保留；清理条件必须再次检查状态和截止时间；
- 租户物理删除、共享事件回收和 sender 脱敏沿既有边界执行，不因为 L3 去重而删除其他租户仍引用的 `canonical_events`；
- 目标频道使用内部 `target_ref` 作为去重分区键。不同目标频道不互相抑制，同一目标的不同租户也不互相抑制；
- 不把 L3 记录扩展成凭据、正文或 Telegram 原始响应归档，不新增敏感字段。

## 三、拟实施范围

### 3.1 数据库与权限

新增幂等迁移及 `migrations-test/` 镜像，具体字段以实施前的现有迁移核对为准，至少覆盖：

- L3 状态、占位任务 id、占位时间与发送确认时间的约束；
- 状态与时间列的 CHECK，禁止 PENDING/CLAIMED/SENT 形状互相伪造；
- 按 `(tenant_id, fingerprint, target_ref)` 和到期时间的索引；
- 现有 app/cleanup 角色的列权限、RLS、审计清单和双遍反证同步更新；
- 不修改已应用迁移，不新增角色，不授予运行时角色读取凭据、正文之外的字段或转授权限。

若实施前发现现有字段可以无迁移地表达完整两阶段状态，也必须先用受限角色验证“并发 claim、失败释放、重启回收”三个形状，不能为了少一份迁移牺牲可证明性。

### 3.2 应用链路

- 在 `crates/app/src/pipeline.rs` 复用现有事件、规则命中和 `target_ref`，计算版本化指纹；
- 在 `crates/db/src/repo.rs` 提供事务内 claim、release 和 confirm 方法，方法参数使用已有 newtype/结构体，避免裸字段混用；
- 在 `crates/app/src/delivery.rs` 将 claim 生命周期接入现有任务状态机和重启回收；
- 保持已有 `telegram_random_id`、请求快照、`RANDOM_ID_DUPLICATE` 和死信语义，不以 L3 claim 替代外部平台幂等；
- 不读取或记录 Bot token、`api_hash`、session、KMS 明文、对象存储密钥或 Telegram 原始错误正文。

### 3.3 测试与证据

至少新增以下可执行测试：

- L1：相同规范输入得到相同指纹；关键词顺序、重复项、空 sender、text/caption 回退和 Unicode 截取均稳定；不同版本前缀不碰撞；
- L2：同一租户/同一目标的并发 claim 恰好一个成功；不同目标、不同租户互不阻塞；失败释放后可重新 claim；SENT 在窗口内抑制，过期后可再次 claim；
- L2：事件正文或规则配置在任务创建后改变，不改变已固化的指纹与投递请求；清理再次核对截止时间；
- L3：无租户上下文、跨租户读取/删除、额外列读取、错误状态写入和角色转授均被拒；RLS 关闭时应用显式租户谓词仍不能跨租户；
- 重启：请求前强杀、外部发送后确认前强杀、响应丢失和 claim 超时恢复，均不留下永久 claim；同一任务沿用原 `random_id`；
- 反证：移除唯一约束、跳过 claim、把释放改为确认或把正文截取规则改回未排序输入时，至少对应测试必须变红；
- 指纹重算：从 `canonical_events` 读取允许字段重新计算，并与 `notify_dedup.fingerprint` 逐条一致；对已脱敏事件明确输出 `not_recomputable`，不伪造一致；
- 完整门禁：在线 SQLx prepare、迁移幂等、workspace test、audit-rls、double-pass 和完整 `ai-gates.sh`，最终以 PR CI 为裁决。

本片不把模拟 transport 当作 Telegram 真机证据。真实限流、退避、在途重复请求和目标频道消息核对继续由隔离测试 Bot、目标频道和脱敏证据完成；没有真机结果时，G4 对应项保持 `not-run`。

## 四、验收输出与停止边界

实施完成后新增一份证据文件，至少包括：基线提交、指纹规范样例、并发 claim 结果、失败/重启结果、重算差异、三类凭据脱敏结果、完整门禁结果和未运行的真实 Telegram 项。证据不写真实 token、`api_hash`、session、KMS ARN 以外的凭据材料、消息正文或个人标识。

本提案及其实现不自动宣布 WP-3、WP-4、G3、G4、WP-8 或生产上线通过；PR #92 合入前不得把 PR #92 的证据矩阵作为本片的新基线。若数据库字段、RLS/GRANT、审计或门禁形状与本提案不同，实施者必须停止并补充提案，不在代码中自行扩大范围。

## 五、审批请求

请批准后实施以下高风险变更：

1. 对既有 `notify_dedup` 增加可证明的 claim/confirm/release 状态形状；
2. 在规则命中到投递任务创建的事务中接入 L3 指纹去重；
3. 将重启、失败、留存、RLS/GRANT、审计和指纹重算测试纳入本片；
4. 在真实 Telegram 验收尚未完成时，保持 G4 和 WP-8 未通过，不创建稳定发布。

[report]
task: G4-L3-notify-dedup
path: ok
gates: pass
escalation: ->
