DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'platform_cleanup') THEN
    CREATE ROLE platform_cleanup LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
  END IF;
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO platform_cleanup', current_database());
END
$$;

SET ROLE tgm_owner;

GRANT USAGE ON SCHEMA public TO platform_cleanup;
GRANT SELECT (tenant_id, id, created_at), DELETE ON system_logs TO platform_cleanup;
GRANT INSERT (tenant_id, level, module, event, context) ON system_logs TO platform_cleanup;
GRANT USAGE ON SEQUENCE system_logs_id_seq TO platform_cleanup;

DROP POLICY IF EXISTS platform_cleanup_select ON system_logs;
CREATE POLICY platform_cleanup_select ON system_logs FOR SELECT TO platform_cleanup
  USING (tenant_id IS NULL AND created_at <= statement_timestamp() - interval '90 days');
DROP POLICY IF EXISTS platform_cleanup_delete ON system_logs;
CREATE POLICY platform_cleanup_delete ON system_logs FOR DELETE TO platform_cleanup
  USING (tenant_id IS NULL AND created_at <= statement_timestamp() - interval '90 days');
DROP POLICY IF EXISTS platform_cleanup_log ON system_logs;
CREATE POLICY platform_cleanup_log ON system_logs FOR INSERT TO platform_cleanup
  WITH CHECK (
    tenant_id IS NULL AND level = 'INFO' AND module = 'platform_retention' AND event = 'cleanup.done'
    AND jsonb_typeof(context) = 'object'
    AND context ?& ARRAY['table', 'rows', 'elapsed_ms']
    AND context - ARRAY['table', 'rows', 'elapsed_ms'] = '{}'::jsonb
    AND context->>'table' = 'system_logs'
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' ~ '^[0-9]+$'
    AND (context->>'rows')::numeric BETWEEN 0 AND 5000
    AND jsonb_typeof(context->'elapsed_ms') = 'number'
    AND context->>'elapsed_ms' ~ '^[0-9]+$'
  );

RESET ROLE;
