SET ROLE tgm_owner;

CREATE FUNCTION tenant_lifecycle_delete_rows(tenant_key BIGINT)
RETURNS TABLE (
  identity_secrets BIGINT,
  credential_keys BIGINT,
  web_sessions BIGINT,
  bot_console_sessions BIGINT,
  login_sessions BIGINT
)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public
AS $$
DECLARE
  remaining_secrets BIGINT;
  remaining_keys BIGINT;
BEGIN
  IF session_user <> 'tenant_lifecycle'
    OR current_setting('app.tenant_id', true) IS DISTINCT FROM tenant_key::text
    OR NOT EXISTS (
      SELECT 1 FROM tenants
       WHERE id = tenant_key AND status = 'deleted'
    ) THEN
    RAISE EXCEPTION 'invalid tenant lifecycle delete context' USING ERRCODE = '42501';
  END IF;

  DELETE FROM identity_secrets secrets
   USING identities identity
   WHERE identity.id = secrets.identity_id AND identity.tenant_id = tenant_key;
  GET DIAGNOSTICS identity_secrets = ROW_COUNT;

  DELETE FROM credential_keys WHERE tenant_id = tenant_key;
  GET DIAGNOSTICS credential_keys = ROW_COUNT;

  DELETE FROM web_sessions WHERE tenant_id = tenant_key;
  GET DIAGNOSTICS web_sessions = ROW_COUNT;

  DELETE FROM bot_console_sessions WHERE tenant_id = tenant_key;
  GET DIAGNOSTICS bot_console_sessions = ROW_COUNT;

  DELETE FROM login_sessions WHERE tenant_id = tenant_key;
  GET DIAGNOSTICS login_sessions = ROW_COUNT;

  SELECT count(*) INTO remaining_secrets
    FROM identity_secrets secrets
    JOIN identities identity ON identity.id = secrets.identity_id
   WHERE identity.tenant_id = tenant_key;
  SELECT count(*) INTO remaining_keys
    FROM credential_keys
   WHERE tenant_id = tenant_key;
  IF remaining_secrets <> 0 OR remaining_keys <> 0 THEN
    RAISE EXCEPTION 'tenant lifecycle credentials remain' USING ERRCODE = '42501';
  END IF;

  RETURN NEXT;
END
$$;

REVOKE ALL ON FUNCTION tenant_lifecycle_delete_rows(BIGINT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION tenant_lifecycle_delete_rows(BIGINT) TO tenant_lifecycle;

RESET ROLE;
