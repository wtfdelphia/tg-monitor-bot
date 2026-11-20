SET ROLE tgm_owner;

ALTER TABLE identities ADD COLUMN IF NOT EXISTS target_probe_not_before TIMESTAMPTZ NOT NULL DEFAULT '-infinity';
GRANT UPDATE (target_probe_not_before) ON identities TO app_user;
ALTER TABLE target_channels
  ADD COLUMN IF NOT EXISTS probe_state TEXT NOT NULL DEFAULT 'PENDING',
  ADD COLUMN IF NOT EXISTS probe_generation BIGINT NOT NULL DEFAULT 1,
  ADD COLUMN IF NOT EXISTS probe_attempts INTEGER NOT NULL DEFAULT 0,
  ADD COLUMN IF NOT EXISTS probe_due_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  ADD COLUMN IF NOT EXISTS probe_token TEXT,
  ADD COLUMN IF NOT EXISTS probe_expires_at TIMESTAMPTZ,
  ADD COLUMN IF NOT EXISTS probe_error_code TEXT,
  ADD COLUMN IF NOT EXISTS content_protected BOOLEAN,
  ADD COLUMN IF NOT EXISTS bot_role TEXT;

DO $$ BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid = 'target_channels'::regclass AND conname = 'target_probe_shape') THEN
    UPDATE target_channels SET can_send = NULL, checked_at = NULL;
    ALTER TABLE target_channels ADD CONSTRAINT target_probe_shape CHECK (
      probe_state IN ('PENDING', 'RUNNING', 'SUCCEEDED', 'FAILED')
      AND probe_generation > 0 AND probe_attempts >= 0
      AND ((probe_state = 'RUNNING') = (probe_token IS NOT NULL AND probe_expires_at IS NOT NULL))
      AND ((probe_token IS NULL) = (probe_expires_at IS NULL))
      AND ((probe_state = 'SUCCEEDED') = (can_send IS NOT NULL AND checked_at IS NOT NULL AND content_protected IS NOT NULL AND bot_role IS NOT NULL))
      AND (probe_state = 'SUCCEEDED' OR (can_send IS NULL AND checked_at IS NULL AND content_protected IS NULL AND bot_role IS NULL))
    );
  END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_target_probe_due ON target_channels (tenant_id, probe_due_at) WHERE probe_state IN ('PENDING', 'RUNNING');

CREATE OR REPLACE FUNCTION invalidate_target_probe() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.tg_chat_id IS DISTINCT FROM OLD.tg_chat_id OR NEW.sender_identity_id IS DISTINCT FROM OLD.sender_identity_id THEN
    NEW.probe_generation := OLD.probe_generation + 1;
    NEW.probe_state := 'PENDING';
    NEW.probe_attempts := 0;
    NEW.probe_due_at := clock_timestamp();
    NEW.probe_token := NULL;
    NEW.probe_expires_at := NULL;
    NEW.probe_error_code := NULL;
    NEW.can_send := NULL;
    NEW.checked_at := NULL;
    NEW.content_protected := NULL;
    NEW.bot_role := NULL;
  END IF;
  RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS target_probe_route_changed ON target_channels;
CREATE TRIGGER target_probe_route_changed BEFORE UPDATE OF tg_chat_id, sender_identity_id ON target_channels
  FOR EACH ROW EXECUTE FUNCTION invalidate_target_probe();

CREATE OR REPLACE FUNCTION invalidate_identity_target_probes() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE identity_key BIGINT; tenant_key BIGINT;
BEGIN
  IF TG_TABLE_NAME = 'identity_secrets' THEN
    identity_key := NEW.identity_id;
    IF TG_OP = 'UPDATE' AND NEW.secret_ciphertext IS NOT DISTINCT FROM OLD.secret_ciphertext
       AND NEW.nonce IS NOT DISTINCT FROM OLD.nonce AND NEW.dek_ciphertext IS NOT DISTINCT FROM OLD.dek_ciphertext
       AND NEW.kek_id IS NOT DISTINCT FROM OLD.kek_id THEN RETURN NEW; END IF;
    SELECT tenant_id INTO tenant_key FROM identities WHERE id = identity_key;
  ELSE
    identity_key := NEW.id;
    tenant_key := NEW.tenant_id;
    IF NEW.kind IS NOT DISTINCT FROM OLD.kind AND NEW.tg_user_id IS NOT DISTINCT FROM OLD.tg_user_id
       AND (NEW.status IN ('banned', 'unauthorized')) = (OLD.status IN ('banned', 'unauthorized')) THEN RETURN NEW; END IF;
  END IF;
  UPDATE target_channels SET probe_generation = probe_generation + 1, probe_state = 'PENDING',
    probe_attempts = 0, probe_due_at = clock_timestamp(), probe_token = NULL, probe_expires_at = NULL,
    probe_error_code = NULL, can_send = NULL, checked_at = NULL, content_protected = NULL, bot_role = NULL
    WHERE tenant_id = tenant_key AND sender_identity_id = identity_key;
  RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS target_probe_credential_changed ON identity_secrets;
CREATE TRIGGER target_probe_credential_changed AFTER INSERT OR UPDATE ON identity_secrets
  FOR EACH ROW EXECUTE FUNCTION invalidate_identity_target_probes();
DROP TRIGGER IF EXISTS target_probe_identity_changed ON identities;
CREATE TRIGGER target_probe_identity_changed AFTER UPDATE OF status, kind, tg_user_id ON identities
  FOR EACH ROW EXECUTE FUNCTION invalidate_identity_target_probes();

RESET ROLE;
