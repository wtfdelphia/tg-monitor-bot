SET ROLE tgm_owner;

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
  IF EXISTS (SELECT 1 FROM tenants WHERE id = tenant_key AND status = 'deleted') THEN
    RETURN NEW;
  END IF;
  UPDATE target_channels SET probe_generation = probe_generation + 1, probe_state = 'PENDING',
    probe_attempts = 0, probe_due_at = clock_timestamp(), probe_token = NULL, probe_expires_at = NULL,
    probe_error_code = NULL, can_send = NULL, checked_at = NULL, content_protected = NULL, bot_role = NULL
    WHERE tenant_id = tenant_key AND sender_identity_id = identity_key;
  RETURN NEW;
END $$;

RESET ROLE;
