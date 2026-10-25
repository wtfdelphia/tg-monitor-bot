SET ROLE tgm_owner;

GRANT USAGE ON SCHEMA public TO tenant_cleanup;
GRANT SELECT (id, plan) ON tenants TO tenant_cleanup;
GRANT SELECT (id, tenant_id) ON delivery_tasks TO tenant_cleanup;
GRANT SELECT (tenant_id, id, expires_at), DELETE ON login_sessions TO tenant_cleanup;
GRANT SELECT (tenant_id, fingerprint, target_ref, expires_at), DELETE ON notify_dedup TO tenant_cleanup;
GRANT SELECT (tenant_id, bot_id, update_id, processed_at), DELETE ON bot_console_updates TO tenant_cleanup;
GRANT SELECT (id, task_id, created_at), DELETE ON delivery_logs TO tenant_cleanup;
GRANT SELECT (tenant_id, id, created_at), DELETE ON audit_logs, system_logs TO tenant_cleanup;
GRANT INSERT (tenant_id, level, module, event, context) ON system_logs TO tenant_cleanup;
GRANT USAGE ON SEQUENCE system_logs_id_seq TO tenant_cleanup;

DROP POLICY IF EXISTS tenant_cleanup_parent ON delivery_tasks;
CREATE POLICY tenant_cleanup_parent ON delivery_tasks FOR SELECT TO tenant_cleanup
  USING (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_cleanup_expired ON login_sessions;
CREATE POLICY tenant_cleanup_expired ON login_sessions FOR ALL TO tenant_cleanup
  USING (tenant_id = current_setting('app.tenant_id')::bigint AND expires_at <= statement_timestamp())
  WITH CHECK (false);
DROP POLICY IF EXISTS tenant_cleanup_expired ON notify_dedup;
CREATE POLICY tenant_cleanup_expired ON notify_dedup FOR ALL TO tenant_cleanup
  USING (tenant_id = current_setting('app.tenant_id')::bigint AND expires_at <= statement_timestamp())
  WITH CHECK (false);
DROP POLICY IF EXISTS tenant_cleanup_expired ON bot_console_updates;
CREATE POLICY tenant_cleanup_expired ON bot_console_updates FOR ALL TO tenant_cleanup
  USING (tenant_id = current_setting('app.tenant_id')::bigint AND processed_at <= statement_timestamp() - interval '7 days')
  WITH CHECK (false);
DROP POLICY IF EXISTS tenant_cleanup_expired ON delivery_logs;
CREATE POLICY tenant_cleanup_expired ON delivery_logs FOR ALL TO tenant_cleanup
  USING (created_at <= statement_timestamp() - interval '90 days' AND EXISTS (
    SELECT 1 FROM delivery_tasks parent WHERE parent.id = delivery_logs.task_id
      AND parent.tenant_id = current_setting('app.tenant_id')::bigint))
  WITH CHECK (false);
DROP POLICY IF EXISTS tenant_cleanup_expired ON audit_logs;
CREATE POLICY tenant_cleanup_expired ON audit_logs FOR ALL TO tenant_cleanup
  USING (tenant_id = current_setting('app.tenant_id')::bigint AND EXISTS (
    SELECT 1 FROM tenants parent WHERE parent.id = audit_logs.tenant_id
      AND audit_logs.created_at <= statement_timestamp() - CASE parent.plan
        WHEN 'free' THEN interval '90 days' WHEN 'pro' THEN interval '365 days' END))
  WITH CHECK (false);
DROP POLICY IF EXISTS tenant_cleanup_expired ON system_logs;
CREATE POLICY tenant_cleanup_expired ON system_logs FOR SELECT TO tenant_cleanup
  USING (tenant_id = current_setting('app.tenant_id')::bigint AND created_at <= statement_timestamp() - interval '90 days');
DROP POLICY IF EXISTS tenant_cleanup_delete ON system_logs;
CREATE POLICY tenant_cleanup_delete ON system_logs FOR DELETE TO tenant_cleanup
  USING (tenant_id = current_setting('app.tenant_id')::bigint AND created_at <= statement_timestamp() - interval '90 days');
DROP POLICY IF EXISTS tenant_cleanup_log ON system_logs;
CREATE POLICY tenant_cleanup_log ON system_logs FOR INSERT TO tenant_cleanup
  WITH CHECK (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND level = 'INFO' AND module = 'tenant_retention' AND event = 'cleanup.done'
    AND jsonb_typeof(context) = 'object'
    AND context ?& ARRAY['table', 'rows', 'elapsed_ms']
    AND context - ARRAY['table', 'rows', 'elapsed_ms'] = '{}'::jsonb
    AND context->>'table' IN ('login_sessions', 'notify_dedup', 'bot_console_updates', 'delivery_logs', 'audit_logs', 'system_logs')
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' ~ '^[0-9]+$'
    AND (context->>'rows')::numeric BETWEEN 0 AND 5000
    AND jsonb_typeof(context->'elapsed_ms') = 'number'
    AND context->>'elapsed_ms' ~ '^[0-9]+$'
  );

CREATE INDEX IF NOT EXISTS idx_login_retention ON login_sessions (tenant_id, expires_at, id);
CREATE INDEX IF NOT EXISTS idx_notify_retention ON notify_dedup (tenant_id, expires_at, fingerprint, target_ref);
CREATE INDEX IF NOT EXISTS idx_bot_updates_retention ON bot_console_updates (tenant_id, processed_at, bot_id, update_id);
CREATE INDEX IF NOT EXISTS idx_delivery_logs_retention ON delivery_logs (task_id, created_at, id);
CREATE INDEX IF NOT EXISTS idx_system_logs_retention ON system_logs (tenant_id, created_at, id);

RESET ROLE;
