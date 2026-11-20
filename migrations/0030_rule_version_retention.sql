SET ROLE tgm_owner;

GRANT SELECT (tenant_id, id, version) ON rules TO tenant_cleanup;
GRANT SELECT (tenant_id, rule_id, version, created_at), DELETE ON rule_versions TO tenant_cleanup;
GRANT SELECT (rule_id, rule_version) ON delivery_tasks TO tenant_cleanup;

DROP POLICY IF EXISTS tenant_cleanup_parent ON rules;
CREATE POLICY tenant_cleanup_parent ON rules FOR SELECT TO tenant_cleanup
  USING (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_cleanup_expired ON rule_versions;
CREATE POLICY tenant_cleanup_expired ON rule_versions FOR ALL TO tenant_cleanup
  USING (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND EXISTS (
      SELECT 1 FROM tenants parent WHERE parent.id = rule_versions.tenant_id
        AND rule_versions.created_at <= statement_timestamp() - CASE parent.plan
          WHEN 'free' THEN interval '90 days' WHEN 'pro' THEN interval '365 days' END)
    AND NOT EXISTS (
      SELECT 1 FROM rules current_rule WHERE current_rule.tenant_id = rule_versions.tenant_id
        AND current_rule.id = rule_versions.rule_id AND current_rule.version = rule_versions.version)
    AND NOT EXISTS (
      SELECT 1 FROM delivery_tasks task WHERE task.tenant_id = rule_versions.tenant_id
        AND task.rule_id = rule_versions.rule_id AND task.rule_version = rule_versions.version)
  ) WITH CHECK (false);

DROP POLICY IF EXISTS tenant_cleanup_log ON system_logs;
CREATE POLICY tenant_cleanup_log ON system_logs FOR INSERT TO tenant_cleanup
  WITH CHECK (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND level = 'INFO' AND module = 'tenant_retention' AND event = 'cleanup.done'
    AND jsonb_typeof(context) = 'object'
    AND context ?& ARRAY['table', 'rows', 'elapsed_ms']
    AND context - ARRAY['table', 'rows', 'elapsed_ms'] = '{}'::jsonb
    AND context->>'table' IN ('login_sessions', 'notify_dedup', 'bot_console_updates', 'delivery_logs', 'audit_logs', 'system_logs', 'rule_versions')
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' ~ '^[0-9]+$'
    AND (context->>'rows')::numeric BETWEEN 0 AND 5000
    AND jsonb_typeof(context->'elapsed_ms') = 'number'
    AND context->>'elapsed_ms' ~ '^[0-9]+$'
  );

CREATE INDEX IF NOT EXISTS idx_tasks_rule_version ON delivery_tasks (tenant_id, rule_id, rule_version);
CREATE INDEX IF NOT EXISTS idx_rule_versions_retention ON rule_versions (tenant_id, created_at, rule_id, version);

RESET ROLE;
