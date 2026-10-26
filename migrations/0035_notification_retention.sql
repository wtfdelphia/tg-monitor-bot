SET ROLE tgm_owner;

ALTER TABLE tenant_notifications ADD COLUMN IF NOT EXISTS terminal_at TIMESTAMPTZ;

RESET ROLE;
UPDATE tenant_notifications
   SET terminal_at = CASE WHEN status = 'SENT' THEN sent_at ELSE statement_timestamp() END
 WHERE status IN ('SENT', 'DEAD') AND terminal_at IS NULL;
SET ROLE tgm_owner;

DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid = 'tenant_notifications'::regclass AND conname = 'tenant_notifications_terminal_check') THEN
    ALTER TABLE tenant_notifications ADD CONSTRAINT tenant_notifications_terminal_check CHECK (
      (status = 'PENDING' AND terminal_at IS NULL)
      OR (status IN ('SENT', 'DEAD') AND terminal_at IS NOT NULL
          AND (status <> 'SENT' OR terminal_at = sent_at))
    );
  END IF;
END $$;

GRANT SELECT (tenant_id, id, status, terminal_at), DELETE ON tenant_notifications TO tenant_cleanup;
DROP POLICY IF EXISTS tenant_cleanup_expired ON tenant_notifications;
CREATE POLICY tenant_cleanup_expired ON tenant_notifications FOR ALL TO tenant_cleanup
  USING (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND status IN ('SENT', 'DEAD')
    AND terminal_at <= statement_timestamp() - interval '90 days'
  ) WITH CHECK (false);

DROP POLICY IF EXISTS tenant_cleanup_log ON system_logs;
CREATE POLICY tenant_cleanup_log ON system_logs FOR INSERT TO tenant_cleanup
  WITH CHECK (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND level = 'INFO' AND module = 'tenant_retention' AND event = 'cleanup.done'
    AND jsonb_typeof(context) = 'object'
    AND context ?& ARRAY['table', 'rows', 'elapsed_ms']
    AND context - ARRAY['table', 'rows', 'elapsed_ms'] = '{}'::jsonb
    AND context->>'table' IN (
      'login_sessions', 'notify_dedup', 'bot_console_updates',
      'delivery_logs', 'audit_logs', 'system_logs', 'rule_versions',
      'tenant_events', 'delivery_tasks', 'tenant_notifications'
    )
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' ~ '^[0-9]+$'
    AND (context->>'rows')::numeric BETWEEN 0 AND 5000
    AND jsonb_typeof(context->'elapsed_ms') = 'number'
    AND context->>'elapsed_ms' ~ '^[0-9]+$'
  );

CREATE INDEX IF NOT EXISTS idx_tenant_notifications_retention
  ON tenant_notifications (tenant_id, terminal_at, id) WHERE status IN ('SENT', 'DEAD');

RESET ROLE;
