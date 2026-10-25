SET ROLE tgm_owner;

GRANT USAGE ON SCHEMA public TO auth_cleanup;
GRANT SELECT (nonce, expires_at), DELETE ON console_login_challenges TO auth_cleanup;
GRANT SELECT (tenant_id, id, expires_at), DELETE ON web_sessions TO auth_cleanup;
GRANT SELECT (tg_user_id, updated_at), DELETE ON bot_console_sessions TO auth_cleanup;
GRANT INSERT (tenant_id, level, module, event, context) ON system_logs TO auth_cleanup;
GRANT USAGE ON SEQUENCE system_logs_id_seq TO auth_cleanup;

DROP POLICY IF EXISTS auth_cleanup_select ON web_sessions;
CREATE POLICY auth_cleanup_select ON web_sessions FOR SELECT TO auth_cleanup
  USING (expires_at <= statement_timestamp());
DROP POLICY IF EXISTS auth_cleanup_delete ON web_sessions;
CREATE POLICY auth_cleanup_delete ON web_sessions FOR DELETE TO auth_cleanup
  USING (expires_at <= statement_timestamp());
DROP POLICY IF EXISTS auth_cleanup_select ON bot_console_sessions;
CREATE POLICY auth_cleanup_select ON bot_console_sessions FOR SELECT TO auth_cleanup
  USING (updated_at <= statement_timestamp() - interval '90 days');
DROP POLICY IF EXISTS auth_cleanup_delete ON bot_console_sessions;
CREATE POLICY auth_cleanup_delete ON bot_console_sessions FOR DELETE TO auth_cleanup
  USING (updated_at <= statement_timestamp() - interval '90 days');

DROP POLICY IF EXISTS auth_cleanup_log ON system_logs;
CREATE POLICY auth_cleanup_log ON system_logs FOR INSERT TO auth_cleanup
  WITH CHECK (
    tenant_id IS NULL AND level = 'INFO' AND module = 'auth_retention' AND event = 'cleanup.done'
    AND jsonb_typeof(context) = 'object'
    AND context ?& ARRAY['table', 'rows', 'elapsed_ms']
    AND context - ARRAY['table', 'rows', 'elapsed_ms'] = '{}'::jsonb
    AND context->>'table' IN ('console_login_challenges', 'web_sessions', 'bot_console_sessions')
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' ~ '^[0-9]+$'
    AND (context->>'rows')::numeric BETWEEN 0 AND 5000
    AND jsonb_typeof(context->'elapsed_ms') = 'number'
    AND context->>'elapsed_ms' ~ '^[0-9]+$'
  );

CREATE INDEX IF NOT EXISTS idx_web_sessions_expire ON web_sessions (expires_at, tenant_id, id);
CREATE INDEX IF NOT EXISTS idx_bot_console_sessions_updated ON bot_console_sessions (updated_at, tg_user_id);

RESET ROLE;
