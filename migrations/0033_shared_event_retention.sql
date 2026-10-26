SET ROLE tgm_owner;

ALTER TABLE tenant_events
  DROP CONSTRAINT IF EXISTS tenant_events_canonical_event_id_fkey;

ALTER TABLE tenant_events
  ADD CONSTRAINT tenant_events_canonical_event_id_fkey
  FOREIGN KEY (canonical_event_id)
  REFERENCES canonical_events (id)
  ON DELETE NO ACTION;

ALTER TABLE media_group_members
  DROP CONSTRAINT IF EXISTS media_group_members_member_event_id_fkey;

ALTER TABLE media_group_members
  ADD CONSTRAINT media_group_members_member_event_id_fkey
  FOREIGN KEY (member_event_id)
  REFERENCES canonical_events (id)
  ON DELETE NO ACTION;

GRANT USAGE ON SCHEMA public TO shared_cleanup;
GRANT SELECT (
  id, receiver_identity_id, tg_chat_id, media_group_id,
  fanned_out, matched_any, created_at
) ON canonical_events TO shared_cleanup;
GRANT DELETE ON canonical_events TO shared_cleanup;
GRANT SELECT (group_event_id, member_event_id) ON media_group_members TO shared_cleanup;
GRANT SELECT (canonical_event_id, status, next_retry_at) ON media_relay_jobs TO shared_cleanup;
GRANT SELECT (canonical_event_id) ON tenant_events TO shared_cleanup;
GRANT INSERT (tenant_id, level, module, event, context) ON system_logs TO shared_cleanup;
GRANT USAGE ON SEQUENCE system_logs_id_seq TO shared_cleanup;

DROP POLICY IF EXISTS shared_cleanup_select ON tenant_events;
CREATE POLICY shared_cleanup_select ON tenant_events FOR SELECT TO shared_cleanup
  USING (
    EXISTS (
      SELECT 1 FROM canonical_events parent
       WHERE parent.id = tenant_events.canonical_event_id
         AND parent.created_at <= statement_timestamp() - interval '24 hours'
    )
  );

DROP POLICY IF EXISTS shared_cleanup_log ON system_logs;
CREATE POLICY shared_cleanup_log ON system_logs FOR INSERT TO shared_cleanup
  WITH CHECK (
    tenant_id IS NULL AND level = 'INFO' AND module = 'shared_retention' AND event = 'cleanup.done'
    AND jsonb_typeof(context) = 'object'
    AND context ?& ARRAY['table', 'rows', 'elapsed_ms']
    AND context - ARRAY['table', 'rows', 'elapsed_ms'] = '{}'::jsonb
    AND context->>'table' = 'canonical_events'
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' ~ '^[0-9]+$'
    AND (context->>'rows')::numeric BETWEEN 0 AND 5000
    AND jsonb_typeof(context->'elapsed_ms') = 'number'
    AND context->>'elapsed_ms' ~ '^[0-9]+$'
  );

RESET ROLE;
