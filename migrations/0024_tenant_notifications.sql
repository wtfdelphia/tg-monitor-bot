-- 系统通知租户（spec/08 §五 第 2 条「通知租户」；docs/plan/proposals/WP-2-remaining.md R2-A，2026-10-22 批准）。
--
-- 身份转入 unauthorized / banned 时，在转状态的同一事务里入队一行 PENDING；
-- control-plane 的发送循环经控制 Bot 私聊 tenants.owner_user_id，成功置 SENT，不可恢复的失败置 DEAD。
-- 不复用 audit_logs 做「已通知」标记：那是执行记录，不应被投递状态回写。
--
-- 甲类（带 tenant_id）：RLS 三件套照 spec/05 §2.2 模板。主键 (tenant_id, id) 满足 A4。
-- identity_id 单列外键：父表 identities.tenant_id 有意可空，A9 不覆盖（与 delivery_tasks 等一致）。
SET ROLE tgm_owner;                        -- 理由见 0002 同一行的注释

CREATE TABLE IF NOT EXISTS tenant_notifications (
  id              BIGINT GENERATED ALWAYS AS IDENTITY,
  tenant_id       BIGINT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  kind            TEXT NOT NULL CHECK (kind IN ('identity.unauthorized','identity.banned')),
  identity_id     BIGINT NOT NULL REFERENCES identities(id) ON DELETE CASCADE,
  status          TEXT NOT NULL DEFAULT 'PENDING' CHECK (status IN ('PENDING','SENT','DEAD')),
  attempt_count   INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
  next_retry_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_error_code TEXT,
  sent_at         TIMESTAMPTZ,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, id),
  -- SENT 必有发送时间，其余状态不得有：发送循环据此判重
  CONSTRAINT tenant_notifications_sent_at_check CHECK ((status = 'SENT') = (sent_at IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_tenant_notifications_due
  ON tenant_notifications (tenant_id, next_retry_at) WHERE status = 'PENDING';

-- 0012 的 ensure_rls event trigger 已自动 ENABLE + FORCE；显式再写一遍，两条 ALTER 幂等
ALTER TABLE tenant_notifications ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_notifications FORCE  ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON tenant_notifications;
CREATE POLICY tenant_isolation ON tenant_notifications TO app_user
  USING      (tenant_id = current_setting('app.tenant_id')::bigint)
  WITH CHECK (tenant_id = current_setting('app.tenant_id')::bigint);

-- 不给 DELETE：随租户、身份级联清理；保留期归 WP-8（spec/04 §一）
GRANT SELECT, INSERT, UPDATE ON tenant_notifications TO app_user;
GRANT USAGE ON SEQUENCE tenant_notifications_id_seq TO app_user;

RESET ROLE;
