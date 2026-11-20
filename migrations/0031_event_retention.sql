SET ROLE tgm_owner;

ALTER TABLE delivery_tasks
  DROP CONSTRAINT IF EXISTS delivery_tasks_tenant_id_tenant_event_id_fkey;

ALTER TABLE delivery_tasks
  ADD CONSTRAINT delivery_tasks_tenant_id_tenant_event_id_fkey
  FOREIGN KEY (tenant_id, tenant_event_id)
  REFERENCES tenant_events (tenant_id, id)
  ON DELETE SET NULL (tenant_event_id);

ALTER TABLE delivery_logs
  DROP CONSTRAINT IF EXISTS delivery_logs_task_id_fkey;

ALTER TABLE delivery_logs
  ADD CONSTRAINT delivery_logs_task_id_fkey
  FOREIGN KEY (task_id)
  REFERENCES delivery_tasks (id)
  ON DELETE RESTRICT;

CREATE OR REPLACE FUNCTION prevent_delivery_payload_snapshot_change() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF ROW(NEW.payload_snapshot, NEW.tenant_id, NEW.id,
         NEW.rule_id, NEW.rule_version, NEW.target_tg_chat_id,
         NEW.sender_identity_id, NEW.action, NEW.telegram_random_id, NEW.idem_key)
     IS DISTINCT FROM
     ROW(OLD.payload_snapshot, OLD.tenant_id, OLD.id,
         OLD.rule_id, OLD.rule_version, OLD.target_tg_chat_id,
         OLD.sender_identity_id, OLD.action, OLD.telegram_random_id, OLD.idem_key)
     OR (NEW.tenant_event_id IS DISTINCT FROM OLD.tenant_event_id
         AND NOT (OLD.tenant_event_id IS NOT NULL AND NEW.tenant_event_id IS NULL)) THEN
    RAISE EXCEPTION 'delivery payload snapshot is immutable';
  END IF;
  RETURN NEW;
END
$$;

GRANT SELECT (tenant_id, id, created_at) ON tenant_events TO tenant_cleanup;
GRANT DELETE ON tenant_events TO tenant_cleanup;
GRANT SELECT (
  id,
  tenant_id,
  tenant_event_id,
  status,
  updated_at
) ON delivery_tasks TO tenant_cleanup;
GRANT UPDATE (tenant_event_id) ON delivery_tasks TO tenant_cleanup;
GRANT DELETE ON delivery_tasks TO tenant_cleanup;

DROP POLICY IF EXISTS tenant_cleanup_detach ON delivery_tasks;
CREATE POLICY tenant_cleanup_detach ON delivery_tasks FOR UPDATE TO tenant_cleanup
  USING (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND tenant_event_id IS NOT NULL
    AND status IN ('DONE', 'DEAD', 'CANCELLED')
  )
  WITH CHECK (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND tenant_event_id IS NULL
    AND status IN ('DONE', 'DEAD', 'CANCELLED')
  );

DROP POLICY IF EXISTS tenant_cleanup_expired ON tenant_events;
CREATE POLICY tenant_cleanup_expired ON tenant_events FOR ALL TO tenant_cleanup
  USING (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND EXISTS (
      SELECT 1 FROM tenants parent
       WHERE parent.id = tenant_events.tenant_id
         AND tenant_events.created_at < statement_timestamp() - CASE parent.plan
           WHEN 'free' THEN interval '7 days'
           WHEN 'pro' THEN interval '30 days'
         END
    )
    AND NOT EXISTS (
      SELECT 1 FROM delivery_tasks task
       WHERE task.tenant_id = tenant_events.tenant_id
         AND task.tenant_event_id = tenant_events.id
         AND task.status IN ('PENDING', 'SENDING', 'FAILED')
    )
  ) WITH CHECK (false);

DROP POLICY IF EXISTS tenant_cleanup_terminal ON delivery_tasks;
CREATE POLICY tenant_cleanup_terminal ON delivery_tasks FOR DELETE TO tenant_cleanup
  USING (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND tenant_event_id IS NULL
    AND status IN ('DONE', 'DEAD', 'CANCELLED')
    AND NOT EXISTS (
      SELECT 1 FROM delivery_logs log
       WHERE log.task_id = delivery_tasks.id
    )
    AND (
      (status = 'DEAD' AND updated_at <= statement_timestamp() - interval '30 days')
      OR (status IN ('DONE', 'DEAD', 'CANCELLED')
          AND updated_at <= statement_timestamp() - interval '90 days')
    )
  );

DROP POLICY IF EXISTS tenant_cleanup_expired ON delivery_logs;
CREATE POLICY tenant_cleanup_expired ON delivery_logs FOR DELETE TO tenant_cleanup
  USING (created_at <= statement_timestamp() - interval '90 days' AND EXISTS (
    SELECT 1 FROM delivery_tasks parent WHERE parent.id = delivery_logs.task_id
      AND parent.tenant_id = current_setting('app.tenant_id')::bigint));
DROP POLICY IF EXISTS tenant_cleanup_delivery_log_parent ON delivery_logs;
CREATE POLICY tenant_cleanup_delivery_log_parent ON delivery_logs FOR SELECT TO tenant_cleanup
  USING (EXISTS (
    SELECT 1 FROM delivery_tasks parent
     WHERE parent.id = delivery_logs.task_id
       AND parent.tenant_id = current_setting('app.tenant_id')::bigint
  ));

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
      'tenant_events', 'delivery_tasks'
    )
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' ~ '^[0-9]+$'
    AND (context->>'rows')::numeric BETWEEN 0 AND 5000
    AND jsonb_typeof(context->'elapsed_ms') = 'number'
    AND context->>'elapsed_ms' ~ '^[0-9]+$'
  );

CREATE INDEX IF NOT EXISTS idx_tenant_events_retention
  ON tenant_events (tenant_id, created_at, id);
CREATE INDEX IF NOT EXISTS idx_delivery_tasks_retention
  ON delivery_tasks (tenant_id, status, tenant_event_id, updated_at, id);

RESET ROLE;
