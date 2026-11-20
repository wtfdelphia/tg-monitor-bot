SET ROLE tgm_owner;

ALTER TABLE tenants ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
UPDATE tenants
   SET deleted_at = statement_timestamp()
 WHERE status = 'deleted' AND deleted_at IS NULL;

CREATE OR REPLACE FUNCTION tenant_suspension_time() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF current_user = 'tenant_lifecycle' THEN
    IF current_setting('app.tenant_id', true) IS NULL
      OR TG_OP <> 'UPDATE'
      OR NEW.id <> current_setting('app.tenant_id', true)::bigint
      OR NOT (
        (OLD.status = 'active' AND NEW.status = 'suspended')
        OR (OLD.status = 'suspended' AND NEW.status = 'active')
        OR (OLD.status = 'suspended' AND NEW.status = 'deleted')
      ) THEN
      RAISE EXCEPTION 'invalid lifecycle transition' USING ERRCODE = '42501';
    END IF;
  END IF;

  IF NEW.status = 'suspended' THEN
    NEW.suspended_at = COALESCE(NEW.suspended_at, statement_timestamp());
    NEW.deleted_at = NULL;
  ELSIF NEW.status = 'deleted' THEN
    NEW.suspended_at = NULL;
    NEW.deleted_at = COALESCE(NEW.deleted_at, statement_timestamp());
  ELSE
    NEW.suspended_at = NULL;
    NEW.deleted_at = NULL;
  END IF;
  RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS tenant_suspension_time ON tenants;
CREATE TRIGGER tenant_suspension_time
BEFORE INSERT OR UPDATE OF status, suspended_at, deleted_at ON tenants
FOR EACH ROW EXECUTE FUNCTION tenant_suspension_time();

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
     WHERE conname = 'tenant_lifecycle_consistent'
       AND conrelid = 'tenants'::regclass
  ) THEN
    ALTER TABLE tenants ADD CONSTRAINT tenant_lifecycle_consistent CHECK (
      (status = 'suspended') = (suspended_at IS NOT NULL)
      AND (status = 'deleted') = (deleted_at IS NOT NULL)
      AND NOT (suspended_at IS NOT NULL AND deleted_at IS NOT NULL)
    );
  END IF;
END $$;

REVOKE UPDATE ON tenants FROM app_user;
GRANT UPDATE(name, updated_at) ON tenants TO app_user;
GRANT SELECT(id, status, suspended_at, deleted_at),
      UPDATE(status, suspended_at, deleted_at, updated_at)
  ON tenants TO tenant_lifecycle;

GRANT UPDATE(status, updated_at) ON identities TO tenant_lifecycle;
GRANT DELETE ON identity_secrets, credential_keys, web_sessions,
  bot_console_sessions, login_sessions TO tenant_lifecycle;
GRANT SELECT(tenant_id) ON credential_keys TO tenant_lifecycle;
GRANT SELECT(tenant_id, id) ON web_sessions TO tenant_lifecycle;
GRANT SELECT(tenant_id, tg_user_id) ON bot_console_sessions TO tenant_lifecycle;
GRANT SELECT(tenant_id, id) ON login_sessions TO tenant_lifecycle;

DROP POLICY IF EXISTS lifecycle_identity_revoke ON identities;
CREATE POLICY lifecycle_identity_revoke ON identities FOR UPDATE TO tenant_lifecycle
  USING (tenant_id = current_setting('app.tenant_id', true)::bigint)
  WITH CHECK (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND status = 'unauthorized'
  );

DROP POLICY IF EXISTS lifecycle_credential_keys_delete ON credential_keys;
CREATE POLICY lifecycle_credential_keys_delete ON credential_keys FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND EXISTS (
      SELECT 1 FROM tenants
       WHERE id = credential_keys.tenant_id AND status = 'deleted'
    )
  );

DROP POLICY IF EXISTS lifecycle_web_sessions_delete ON web_sessions;
CREATE POLICY lifecycle_web_sessions_delete ON web_sessions FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND EXISTS (
      SELECT 1 FROM tenants
       WHERE id = web_sessions.tenant_id AND status = 'deleted'
    )
  );

DROP POLICY IF EXISTS lifecycle_bot_sessions_delete ON bot_console_sessions;
CREATE POLICY lifecycle_bot_sessions_delete ON bot_console_sessions FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND EXISTS (
      SELECT 1 FROM tenants
       WHERE id = bot_console_sessions.tenant_id AND status = 'deleted'
    )
  );

DROP POLICY IF EXISTS lifecycle_login_sessions_delete ON login_sessions;
CREATE POLICY lifecycle_login_sessions_delete ON login_sessions FOR DELETE TO tenant_lifecycle
  USING (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND EXISTS (
      SELECT 1 FROM tenants
       WHERE id = login_sessions.tenant_id AND status = 'deleted'
    )
  );

DROP POLICY IF EXISTS lifecycle_audit ON audit_logs;
CREATE POLICY lifecycle_audit ON audit_logs FOR INSERT TO tenant_lifecycle
  WITH CHECK (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND source = 'system'
    AND resource = 'tenant'
    AND resource_id = tenant_id
    AND (
      (action = 'tenant.suspend'
       AND before = '{"status":"active"}'::jsonb
       AND after = '{"status":"suspended"}'::jsonb)
      OR (action = 'tenant.resume'
       AND before = '{"status":"suspended"}'::jsonb
       AND after = '{"status":"active"}'::jsonb)
      OR (action = 'tenant.delete'
       AND before = '{"status":"suspended"}'::jsonb
       AND after = '{"status":"deleted"}'::jsonb)
    )
  );

DROP POLICY IF EXISTS lifecycle_log ON system_logs;
CREATE POLICY lifecycle_log ON system_logs FOR INSERT TO tenant_lifecycle
  WITH CHECK (
    tenant_id = current_setting('app.tenant_id', true)::bigint
    AND level = 'INFO'
    AND module = 'tenant_lifecycle'
    AND (
      (
        event = 'cleanup.done'
        AND jsonb_typeof(context) = 'object'
        AND context ?& ARRAY['table', 'rows', 'elapsed_ms']
        AND context - ARRAY['table', 'rows', 'elapsed_ms'] = '{}'::jsonb
        AND context->>'table' = 'delivery_tasks'
        AND jsonb_typeof(context->'rows') = 'number'
        AND context->>'rows' ~ '^[0-9]+$'
        AND (context->>'rows')::numeric BETWEEN 0 AND 5000
        AND jsonb_typeof(context->'elapsed_ms') = 'number'
        AND context->>'elapsed_ms' ~ '^[0-9]+$'
      )
      OR (
        event = 'delete.done'
        AND jsonb_typeof(context) = 'object'
        AND context ?& ARRAY[
          'identities', 'identity_secrets', 'credential_keys',
          'web_sessions', 'bot_console_sessions', 'login_sessions'
        ]
        AND context - ARRAY[
          'identities', 'identity_secrets', 'credential_keys',
          'web_sessions', 'bot_console_sessions', 'login_sessions'
        ] = '{}'::jsonb
        AND jsonb_typeof(context->'identities') = 'number'
        AND jsonb_typeof(context->'identity_secrets') = 'number'
        AND jsonb_typeof(context->'credential_keys') = 'number'
        AND jsonb_typeof(context->'web_sessions') = 'number'
        AND jsonb_typeof(context->'bot_console_sessions') = 'number'
        AND jsonb_typeof(context->'login_sessions') = 'number'
        AND (context->>'identities')::numeric BETWEEN 0 AND 5000
        AND (context->>'identity_secrets')::numeric BETWEEN 0 AND 5000
        AND (context->>'credential_keys')::numeric BETWEEN 0 AND 1
        AND (context->>'web_sessions')::numeric BETWEEN 0 AND 5000
        AND (context->>'bot_console_sessions')::numeric BETWEEN 0 AND 5000
        AND (context->>'login_sessions')::numeric BETWEEN 0 AND 5000
      )
    )
  );

CREATE OR REPLACE FUNCTION tenant_lifecycle_secret_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF current_user = 'tenant_lifecycle'
    AND (
      current_setting('app.tenant_id', true) IS NULL
      OR NOT EXISTS (
        SELECT 1 FROM identities
         WHERE id = OLD.identity_id
           AND tenant_id = current_setting('app.tenant_id', true)::bigint
      )
    ) THEN
    RAISE EXCEPTION 'invalid lifecycle secret delete' USING ERRCODE = '42501';
  END IF;
  RETURN OLD;
END
$$;

DROP TRIGGER IF EXISTS tenant_lifecycle_secret_delete ON identity_secrets;
CREATE TRIGGER tenant_lifecycle_secret_delete
BEFORE DELETE ON identity_secrets
FOR EACH ROW EXECUTE FUNCTION tenant_lifecycle_secret_delete();

RESET ROLE;
