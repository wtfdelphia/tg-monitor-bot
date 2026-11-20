SET ROLE tgm_owner;

GRANT SELECT (canonical_event_id, processed_at, matched) ON tenant_events TO tenant_cleanup;
GRANT SELECT (id, created_at, fanned_out) ON canonical_events TO tenant_cleanup;

DROP POLICY IF EXISTS tenant_cleanup_event_metadata ON tenant_events;
CREATE POLICY tenant_cleanup_event_metadata ON tenant_events FOR SELECT TO tenant_cleanup
  USING (tenant_id = current_setting('app.tenant_id')::bigint);

DROP POLICY IF EXISTS tenant_cleanup_unmatched ON tenant_events;
CREATE POLICY tenant_cleanup_unmatched ON tenant_events FOR DELETE TO tenant_cleanup
  USING (
    tenant_id = current_setting('app.tenant_id')::bigint
    AND processed_at IS NOT NULL
    AND NOT matched
    AND EXISTS (
      SELECT 1 FROM canonical_events parent
       WHERE parent.id = tenant_events.canonical_event_id
         AND parent.fanned_out
         AND parent.created_at < statement_timestamp() - interval '24 hours'
    )
    AND NOT EXISTS (
      SELECT 1 FROM delivery_tasks task
       WHERE task.tenant_id = tenant_events.tenant_id
         AND task.tenant_event_id = tenant_events.id
    )
  );

CREATE INDEX IF NOT EXISTS idx_tenant_events_unmatched_retention
  ON tenant_events (tenant_id, canonical_event_id, id)
  WHERE processed_at IS NOT NULL AND NOT matched;

RESET ROLE;
