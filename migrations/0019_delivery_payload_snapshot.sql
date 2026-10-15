SET ROLE tgm_owner;

ALTER TABLE delivery_tasks
  ADD COLUMN IF NOT EXISTS payload_snapshot JSONB;

ALTER TABLE delivery_tasks
  DROP CONSTRAINT IF EXISTS delivery_tasks_payload_snapshot_check;

ALTER TABLE delivery_tasks
  ADD CONSTRAINT delivery_tasks_payload_snapshot_check
  CHECK (
    payload_snapshot IS NULL OR COALESCE((
      telegram_random_id IS NOT NULL
      AND
      jsonb_typeof(payload_snapshot) = 'object'
      AND jsonb_typeof(payload_snapshot -> 'source') = 'object'
      AND COALESCE(
        jsonb_typeof(payload_snapshot -> 'source' -> 'tg_chat_id') = 'number'
        AND (payload_snapshot -> 'source' ->> 'tg_chat_id') ~ '^-?[0-9]+$',
        FALSE
      )
      AND COALESCE(
        jsonb_typeof(payload_snapshot -> 'source' -> 'tg_message_id') = 'number'
        AND (payload_snapshot -> 'source' ->> 'tg_message_id') ~ '^[0-9]+$',
        FALSE
      )
      AND COALESCE((
        jsonb_typeof(payload_snapshot -> 'source' -> 'media_type') = 'string'
        OR jsonb_typeof(payload_snapshot -> 'source' -> 'media_type') = 'null'
      ), FALSE)
      AND jsonb_typeof(payload_snapshot -> 'message') = 'object'
      AND COALESCE(jsonb_typeof(payload_snapshot -> 'message' -> 'text') = 'string', FALSE)
      AND COALESCE(payload_snapshot -> 'message' ->> 'parse_mode' = 'HTML', FALSE)
    ), FALSE)
  );

CREATE OR REPLACE FUNCTION prevent_delivery_payload_snapshot_change() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF ROW(NEW.payload_snapshot, NEW.tenant_id, NEW.id, NEW.tenant_event_id,
         NEW.rule_id, NEW.rule_version, NEW.target_tg_chat_id,
         NEW.sender_identity_id, NEW.action, NEW.telegram_random_id, NEW.idem_key)
     IS DISTINCT FROM
     ROW(OLD.payload_snapshot, OLD.tenant_id, OLD.id, OLD.tenant_event_id,
         OLD.rule_id, OLD.rule_version, OLD.target_tg_chat_id,
         OLD.sender_identity_id, OLD.action, OLD.telegram_random_id, OLD.idem_key) THEN
    RAISE EXCEPTION 'delivery payload snapshot is immutable';
  END IF;
  RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS delivery_payload_snapshot_immutable ON delivery_tasks;
CREATE TRIGGER delivery_payload_snapshot_immutable
  BEFORE UPDATE ON delivery_tasks
  FOR EACH ROW EXECUTE FUNCTION prevent_delivery_payload_snapshot_change();

REVOKE UPDATE ON delivery_tasks FROM app_user;
GRANT UPDATE (
  status,
  attempt_count,
  next_retry_at,
  last_error_code,
  telegram_message_id,
  completed_at,
  updated_at
) ON delivery_tasks TO app_user;

RESET ROLE;
