SET ROLE tgm_owner;

CREATE OR REPLACE FUNCTION tenant_lifecycle_tenant_is_deleted(tenant_key BIGINT)
RETURNS BOOLEAN
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = public
AS $$
  SELECT EXISTS (
    SELECT 1 FROM tenants
     WHERE id = tenant_key AND status = 'deleted'
  )
$$;

REVOKE ALL ON FUNCTION tenant_lifecycle_tenant_is_deleted(BIGINT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION tenant_lifecycle_tenant_is_deleted(BIGINT) TO tenant_lifecycle;

DROP POLICY IF EXISTS lifecycle_credential_keys_delete ON credential_keys;
CREATE POLICY lifecycle_credential_keys_delete ON credential_keys FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND tenant_lifecycle_tenant_is_deleted(tenant_id)
  );

DROP POLICY IF EXISTS lifecycle_web_sessions_delete ON web_sessions;
CREATE POLICY lifecycle_web_sessions_delete ON web_sessions FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND tenant_lifecycle_tenant_is_deleted(tenant_id)
  );

DROP POLICY IF EXISTS lifecycle_bot_sessions_delete ON bot_console_sessions;
CREATE POLICY lifecycle_bot_sessions_delete ON bot_console_sessions FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND tenant_lifecycle_tenant_is_deleted(tenant_id)
  );

DROP POLICY IF EXISTS lifecycle_login_sessions_delete ON login_sessions;
CREATE POLICY lifecycle_login_sessions_delete ON login_sessions FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND tenant_lifecycle_tenant_is_deleted(tenant_id)
  );

RESET ROLE;
