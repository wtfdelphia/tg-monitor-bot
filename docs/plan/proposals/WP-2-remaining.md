# WP-2-remaining：WP-2 包内剩余三项

状态：2026-10-22 批准方向：R1 现在做；R2 走方案 A；R3 不在 WP-2 内做。R2 的 DDL 见 §R2-A 细化，**等第二次批准**；`USER_DEACTIVATED*` 拆分见同节。

## 任务与判据

任务 id：`WP-2-remaining`。

判据出处：`docs/plan/evidence/WP-2-出口判定-2026-10-21.md` §四 的剩余三项：

| 项 | 判据出处 | 命中的升级条件 |
|---|---|---|
| R1 运行中周期授权校验识别撤销（真机） | spec/08 §五；`WP-2-qr-login-live-2026-10-21.md` T6 的未覆盖说明 | AGENTS §五第 5 条（真机、需撤销会话） |
| R2 转入 `unauthorized` / `banned` 时通知租户 | spec/08 §五 三件事的第 2 条 | 视方案而定，见下文；可能命中第 1、3 条 |
| R3 spec/08 §4.5 混沌五项 | spec/08 §4.5；eng/03 X1–X5；ops/01 §一 | 不建议现在做，见下文 |

## R1：运行中撤销（真机）

现状：`run_leased` 每 `auth_check_interval`（默认 60s）调一次 `check_authorized`。T6 识别撤销的那一次发生在上线前的校验，
在线期间被撤销、到下一个周期才识别出来的这条分支没有真机证据。

做法：在 `crates/app/tests/mtproto_live.rs` 新增 ignored 用例 `live_r1_revoked_while_online`。
worker 用 `auth_check_interval = 10s` 上线，打出 `[R1] online, revoke now` 后等人在手机端结束这台设备的会话，
判据是 180 秒内退出 `Unauthorized`、`identities.status = unauthorized`、`identity.unauthorized` 审计恰好一条、租约可被接管。
同时记录从人回复「已撤销」到识别出来的耗时。

不改生产代码，不加迁移和依赖。需要你配合：扫码一次，worker 上线后在手机端撤销。会话随之永久失效。

## R2：租户通知

### 现状

- `mark_leased_identity_unauthorized`（`crates/db/src/repo.rs`）在一个持租约事务里完成转状态、释放租约和写审计（`identity.unauthorized`，`source='system'`），没有通知。
- `banned` 目前没有任何代码路径会转入：`is_unauthorized` 把 `USER_DEACTIVATED*` 也归到了 `unauthorized`。
- 现有的对租户可见通道只有两个：控制台 dashboard 的 `identities.unauthorized` 计数，以及控制 Bot（`TelegramBotApiClient::send_status_message`，只在 control-plane 上配置了 token）。
- spec/03 §6.1 对投递 `DEAD` 也写了「通知租户」，同样没有实现。两者需要同一个通知通道。
- 没有通知相关的表。`notify_dedup` 是消息级去重（L3），不是系统通知。

### 设计空白（需要你定）

spec 只写了「通知租户」，没有定义通道、对象、重试和去重。可选方案如下：

| 方案 | 做法 | 迁移 / 安全面 | 代价 |
|---|---|---|---|
| A 控制 Bot 私聊 owner（推荐） | control-plane 后台每 30s 扫描 `audit_logs` 中尚未通知的 `identity.unauthorized`，经控制 Bot 给 `tenants.owner_user_id` 发私聊 | 需要记录「已通知」：新增一张 `tenant_notifications`（`tenant_id`、`audit_log_id` 唯一、`sent_at`、`last_error`），带 RLS 三件套。命中第 1、3 条 | 每家 owner 必须先和控制 Bot 有过私聊（扫码登录时已有）；Telegram 返回 403 时只能记错误 |
| B 只在控制台展示 | dashboard 与身份页突出显示 `unauthorized`，`audit_logs` 增加租户可读的筛选 | 不加迁移；只改 control-plane 和控制台 | 不是主动推送，用户不上控制台就不知道，正是 spec/03 §6.1 说的「投递停了用户完全不知道」 |
| C 先 B 后 A | 本片做 B，A 单独立项，和 DEAD 通知一起做 | 本片不加迁移 | 主动通知要晚一个任务才有 |

我推荐 A，但它要建新表，并且要定通知的文案和语种。按 AGENTS §五第 1 条，批准前不建表。
如果选 A，我先把 DDL、RLS 和 GRANT 的具体形状补进本提案，等第二次批准再实现。

额外需要决定：`USER_DEACTIVATED*` 是否从 `unauthorized` 拆出来，单独转入 `banned`。目前两者对外表现相同（都不重试），
拆开后通知文案才能区分「重新扫码」和「账号已被封」。

## R3：混沌五项

spec/08 §4.5 明确写了它是「多实例上线的前置条件」；ops/01 §一 写明首发形态 A 下用「单实例租约防误双开验证」替代，
混沌五项推迟到演进阶段。首发不需要，现在做也解除不了任何首发阻塞。

建议：本任务不做 R3。首发前要做的是 ops/01 §一 的「单实例租约防误双开验证」，它不在 WP-2，归到上线关口另行立项。

## 需要你决定的事

1. R1：是否现在做（需要扫码，并撤销一次会话）。
2. R2：选 A、B 还是 C。选 A 的话，我先补 DDL 提案再等第二次批准。
3. R2：`USER_DEACTIVATED*` 是否拆到 `banned`。
4. R3：是否同意不在 WP-2 内做，改为上线关口的「防误双开验证」另行立项。

## 批准记录（2026-10-22）

用户回复「批准」，按推荐项执行：

1. R1：已完成（2026-10-22 真机通过，证据 `docs/plan/evidence/WP-2-R1-在线撤销-2026-10-22.md`）。
2. R2：方案 A。下方 §R2-A 细化是 DDL 形状，**实现前需第二次批准**（AGENTS §五第 1、3 条）。
3. `USER_DEACTIVATED*`：本节建议拆分，随 R2-A 一并批准。
4. R3：不在 WP-2 内做，归上线关口的「单实例租约防误双开验证」另行立项。

## R2-A 细化（待第二次批准）

### 迁移 `0024_tenant_notifications.sql`

```sql
SET ROLE tgm_owner;

CREATE TABLE IF NOT EXISTS tenant_notifications (
  id            BIGINT GENERATED ALWAYS AS IDENTITY,
  tenant_id     BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  kind          TEXT NOT NULL CHECK (kind IN ('identity.unauthorized','identity.banned')),
  identity_id   BIGINT NOT NULL REFERENCES identities(id) ON DELETE CASCADE,
  status        TEXT NOT NULL DEFAULT 'PENDING' CHECK (status IN ('PENDING','SENT','DEAD')),
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  next_retry_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_error_code TEXT,
  sent_at       TIMESTAMPTZ,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, id),
  CHECK ((status = 'SENT') = (sent_at IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_tenant_notifications_due
  ON tenant_notifications (next_retry_at) WHERE status = 'PENDING';

-- 0012 的 event trigger 已自动 ENABLE + FORCE；这里显式写一遍，两条 ALTER 幂等
ALTER TABLE tenant_notifications ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_notifications FORCE  ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON tenant_notifications;
CREATE POLICY tenant_isolation ON tenant_notifications TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

GRANT SELECT, INSERT, UPDATE ON tenant_notifications TO app_user;
GRANT USAGE ON SEQUENCE tenant_notifications_id_seq TO app_user;

RESET ROLE;
```

形状说明：

- 甲类表，RLS 三件套照 spec/05 模板；主键 `(tenant_id, id)` 满足 A4。
- `identity_id` 单列外键：父表 `identities.tenant_id` 有意可空，A9 自动不覆盖（与现有表一致）。
- 不给 DELETE：随租户、身份级联清理；留存期归 WP-8。
- 不复用 `audit_logs` 做「已通知」标记：`audit_logs` 是执行记录，不应被投递状态回写。

### 写入与发送

- 入队：在 `mark_leased_identity_unauthorized` 的同一持租约事务里 INSERT 一行 PENDING。转状态、释放租约、写审计、入队，四件事要么一起成功，要么一起回滚。spec/08 §五的三件事因此都在库层落地。
- 发送：control-plane 新增后台循环（每 30s），按 `list_all_active_tenants` 逐租户在 `tenant_tx` 内取到期的 PENDING，经控制 Bot `send_status_message` 私聊 `tenants.owner_user_id`。
  - 成功：置 SENT。
  - 403（用户未私聊 Bot、已拉黑）或 400：直接置 DEAD，记 `last_error_code`，不重试。
  - 429、5xx、网络错误：指数退避，最多 5 次，之后置 DEAD。
- 未配置 `TGM_CONTROL_BOT_TOKEN` 时不启动发送循环，行留在 PENDING，控制台仍可见。
- 文案（中文，首发单语种）：「监控账号 #{identity_id} 的 Telegram 授权已失效，采集已停止。请在控制台重新扫码登录。」banned 用「已被 Telegram 封禁，无法自动恢复」。不含任何 session、手机号等敏感信息。

### `USER_DEACTIVATED*` 拆分

- `crates/tg/src/client.rs` 新增 `is_banned()`，只认 `USER_DEACTIVATED` / `USER_DEACTIVATED_BAN`；`is_unauthorized()` 去掉这一项。
- worker 的三处匹配加 `is_banned` 分支，走新的 `mark_leased_identity_banned`（与 unauthorized 同一形状：转 `banned`、释放租约、写 `identity.banned` 审计、入队通知）。
- 封禁只能由 Telegram 返回的错误码判定；没有真机可以复现，只做 L1/L2 测试，在证据里写明没有真机覆盖。

### 测试

- L2：入队与转状态同事务（租约丢失时都不写）；RLS 隔离以 `app_user` 连接，租户 B 读不到 A 的通知；403 置 DEAD 不重试；429 退避；发送成功置 SENT 且不重复发送。
- 审计：`audit-rls` 自动覆盖新表；`audit-double-pass.sh` 照常跑。
- 真机：R1 撤销后经测试控制 Bot 实际收到私聊（可选，需要你提供控制 Bot token，并先与它私聊过）。
