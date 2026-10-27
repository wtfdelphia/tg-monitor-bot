DO $$
BEGIN
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO media_cleanup', current_database());
END
$$;

SET ROLE tgm_owner;

GRANT USAGE ON SCHEMA public TO media_cleanup;
GRANT SELECT (id, object_key, object_expires_at, first_seen_at),
  UPDATE (object_key, object_expires_at), DELETE ON media_archive TO media_cleanup;
GRANT SELECT (media_archive_id) ON media_relay_jobs TO media_cleanup;
GRANT INSERT (tenant_id, level, module, event, context) ON system_logs TO media_cleanup;
GRANT USAGE ON SEQUENCE system_logs_id_seq TO media_cleanup;

DROP POLICY IF EXISTS media_cleanup_log ON system_logs;
CREATE POLICY media_cleanup_log ON system_logs FOR INSERT TO media_cleanup
  WITH CHECK (
    tenant_id IS NULL AND level = 'INFO' AND module = 'media_retention' AND event = 'cleanup.done'
    AND jsonb_typeof(context) = 'object'
    AND context ?& ARRAY['table', 'rows', 'elapsed_ms']
    AND context - ARRAY['table', 'rows', 'elapsed_ms'] = '{}'::jsonb
    AND context->>'table' IN ('media_archive.object', 'media_archive', 'staging')
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' ~ '^[0-9]+$'
    AND (context->>'rows')::numeric BETWEEN 0 AND 5000
    AND jsonb_typeof(context->'elapsed_ms') = 'number'
    AND context->>'elapsed_ms' ~ '^[0-9]+$'
  );

RESET ROLE;
