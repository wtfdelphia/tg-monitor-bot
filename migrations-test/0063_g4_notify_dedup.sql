-- G4 L3 通知去重：为现有指纹行增加发送 claim 生命周期。
SET ROLE tgm_owner;

ALTER TABLE notify_dedup
  ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'SENT';
ALTER TABLE notify_dedup
  ADD COLUMN IF NOT EXISTS task_id BIGINT;
ALTER TABLE notify_dedup
  ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ;
ALTER TABLE notify_dedup
  ALTER COLUMN last_sent_at DROP NOT NULL;

ALTER TABLE delivery_tasks
  ADD COLUMN IF NOT EXISTS notification_fingerprint TEXT;
ALTER TABLE delivery_tasks
  ADD COLUMN IF NOT EXISTS notification_target_ref BIGINT;

ALTER TABLE notify_dedup
  DROP CONSTRAINT IF EXISTS notify_dedup_task_id_fkey;

ALTER TABLE delivery_tasks
  DROP CONSTRAINT IF EXISTS delivery_tasks_tenant_id_id_key;
ALTER TABLE delivery_tasks
  ADD CONSTRAINT delivery_tasks_tenant_id_id_key UNIQUE (tenant_id, id);

ALTER TABLE notify_dedup
  ADD CONSTRAINT notify_dedup_task_id_fkey
  FOREIGN KEY (tenant_id, task_id)
  REFERENCES delivery_tasks (tenant_id, id) ON DELETE CASCADE;

ALTER TABLE notify_dedup DROP CONSTRAINT IF EXISTS notify_dedup_status_check;
ALTER TABLE notify_dedup ADD CONSTRAINT notify_dedup_status_check CHECK (
  (status = 'CLAIMED' AND task_id IS NOT NULL AND claimed_at IS NOT NULL AND last_sent_at IS NULL)
  OR (status = 'SENT' AND task_id IS NULL AND claimed_at IS NULL AND last_sent_at IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS idx_notify_claim
  ON notify_dedup (tenant_id, fingerprint, target_ref, expires_at, status);
CREATE INDEX IF NOT EXISTS idx_notify_task ON notify_dedup (tenant_id, task_id)
  WHERE task_id IS NOT NULL;
DROP INDEX IF EXISTS uq_notify_task;
CREATE UNIQUE INDEX uq_notify_task
  ON notify_dedup (tenant_id, task_id)
  WHERE task_id IS NOT NULL;

GRANT SELECT (tenant_id, fingerprint, target_ref, status, expires_at), DELETE
  ON notify_dedup TO tenant_cleanup;

DROP POLICY IF EXISTS tenant_cleanup_expired ON notify_dedup;
CREATE POLICY tenant_cleanup_expired ON notify_dedup FOR ALL TO tenant_cleanup
  USING (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND status = 'SENT'
    AND expires_at <= statement_timestamp()
  )
  WITH CHECK (false);

RESET ROLE;
