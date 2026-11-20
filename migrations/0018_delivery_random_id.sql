SET ROLE tgm_owner;

ALTER TABLE delivery_tasks
  ADD COLUMN IF NOT EXISTS telegram_random_id BIGINT;

ALTER TABLE delivery_tasks
  DROP CONSTRAINT IF EXISTS delivery_tasks_telegram_random_id_check;

ALTER TABLE delivery_tasks
  ADD CONSTRAINT delivery_tasks_telegram_random_id_check
  CHECK (telegram_random_id IS NULL OR telegram_random_id > 0);

CREATE UNIQUE INDEX IF NOT EXISTS uq_task_telegram_random_id
  ON delivery_tasks (tenant_id, sender_identity_id, target_tg_chat_id, telegram_random_id)
  WHERE telegram_random_id IS NOT NULL;

CREATE OR REPLACE FUNCTION prevent_delivery_random_id_change() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF OLD.telegram_random_id IS NOT NULL
     AND NEW.telegram_random_id IS DISTINCT FROM OLD.telegram_random_id THEN
    RAISE EXCEPTION 'telegram_random_id is immutable after assignment';
  END IF;
  RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS delivery_random_id_immutable ON delivery_tasks;
CREATE TRIGGER delivery_random_id_immutable
  BEFORE UPDATE OF telegram_random_id ON delivery_tasks
  FOR EACH ROW EXECUTE FUNCTION prevent_delivery_random_id_change();

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
