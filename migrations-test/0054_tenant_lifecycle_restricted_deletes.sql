SET ROLE tgm_owner;

DROP FUNCTION IF EXISTS tenant_lifecycle_delete_rows(BIGINT);

SET ROLE tgm_owner;

DROP POLICY IF EXISTS lifecycle_credential_keys_delete ON credential_keys;
CREATE POLICY lifecycle_credential_keys_delete ON credential_keys FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND EXISTS (SELECT 1 FROM tenants WHERE id = credential_keys.tenant_id AND status = 'deleted')
  );
DROP POLICY IF EXISTS lifecycle_credential_keys_read ON credential_keys;
CREATE POLICY lifecycle_credential_keys_read ON credential_keys FOR SELECT TO tenant_lifecycle
  USING (tenant_id = current_setting('app.tenant_id', true)::bigint);

DROP POLICY IF EXISTS lifecycle_web_sessions_delete ON web_sessions;
CREATE POLICY lifecycle_web_sessions_delete ON web_sessions FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND EXISTS (SELECT 1 FROM tenants WHERE id = web_sessions.tenant_id AND status = 'deleted')
  );
DROP POLICY IF EXISTS lifecycle_web_sessions_read ON web_sessions;
CREATE POLICY lifecycle_web_sessions_read ON web_sessions FOR SELECT TO tenant_lifecycle
  USING (tenant_id = current_setting('app.tenant_id', true)::bigint);

DROP POLICY IF EXISTS lifecycle_bot_sessions_delete ON bot_console_sessions;
CREATE POLICY lifecycle_bot_sessions_delete ON bot_console_sessions FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND EXISTS (SELECT 1 FROM tenants WHERE id = bot_console_sessions.tenant_id AND status = 'deleted')
  );
DROP POLICY IF EXISTS lifecycle_bot_sessions_read ON bot_console_sessions;
CREATE POLICY lifecycle_bot_sessions_read ON bot_console_sessions FOR SELECT TO tenant_lifecycle
  USING (tenant_id = current_setting('app.tenant_id', true)::bigint);

DROP POLICY IF EXISTS lifecycle_login_sessions_delete ON login_sessions;
CREATE POLICY lifecycle_login_sessions_delete ON login_sessions FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND EXISTS (SELECT 1 FROM tenants WHERE id = login_sessions.tenant_id AND status = 'deleted')
  );
DROP POLICY IF EXISTS lifecycle_login_sessions_read ON login_sessions;
CREATE POLICY lifecycle_login_sessions_read ON login_sessions FOR SELECT TO tenant_lifecycle
  USING (tenant_id = current_setting('app.tenant_id', true)::bigint);

DROP POLICY IF EXISTS lifecycle_identity_revoke ON identities;
CREATE POLICY lifecycle_identity_revoke ON identities FOR UPDATE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND EXISTS (SELECT 1 FROM tenants WHERE id = identities.tenant_id AND status = 'deleted')
  )
  WITH CHECK (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND status = 'unauthorized'
    AND EXISTS (SELECT 1 FROM tenants WHERE id = identities.tenant_id AND status = 'deleted')
  );

CREATE OR REPLACE FUNCTION tenant_lifecycle_secret_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF current_user = 'tenant_lifecycle'
    AND (
      current_setting('app.tenant_id', true) IS NULL
      OR NOT EXISTS (
        SELECT 1 FROM identities identity JOIN tenants tenant ON tenant.id = identity.tenant_id
         WHERE identity.id = OLD.identity_id
           AND identity.tenant_id = current_setting('app.tenant_id', true)::bigint
           AND tenant.status = 'deleted'
      )
    ) THEN
    RAISE EXCEPTION 'invalid lifecycle secret delete' USING ERRCODE = '42501';
  END IF;
  RETURN OLD;
END
$$;

DROP FUNCTION IF EXISTS tenant_lifecycle_tenant_is_deleted(BIGINT);

RESET ROLE;
