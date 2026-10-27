DO $$
BEGIN
  IF NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname='tenant_lifecycle') THEN
    CREATE ROLE tenant_lifecycle LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
  END IF;
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO tenant_lifecycle',current_database());
END
$$;
SET ROLE tgm_owner;
ALTER TABLE tenants ADD COLUMN IF NOT EXISTS suspended_at TIMESTAMPTZ;
UPDATE tenants SET suspended_at=statement_timestamp() WHERE status='suspended' AND suspended_at IS NULL;
CREATE OR REPLACE FUNCTION tenant_suspension_time() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF current_user='tenant_lifecycle' THEN
    IF TG_OP<>'UPDATE' OR NEW.id<>current_setting('app.tenant_id')::bigint OR
      NOT ((OLD.status='active' AND NEW.status='suspended') OR (OLD.status='suspended' AND NEW.status='active')) THEN
      RAISE EXCEPTION 'invalid lifecycle transition' USING ERRCODE='42501';
    END IF;
    NEW.suspended_at=CASE WHEN NEW.status='suspended' THEN statement_timestamp() ELSE NULL END;
  END IF;
  IF NEW.status='suspended' THEN
    NEW.suspended_at=COALESCE(NEW.suspended_at,statement_timestamp());
  ELSE
    NEW.suspended_at=NULL;
  END IF;
  RETURN NEW;
END
$$;
DROP TRIGGER IF EXISTS tenant_suspension_time ON tenants;
CREATE TRIGGER tenant_suspension_time BEFORE INSERT OR UPDATE OF status,suspended_at ON tenants
  FOR EACH ROW EXECUTE FUNCTION tenant_suspension_time();
DO $$ BEGIN
  IF NOT EXISTS(SELECT 1 FROM pg_constraint WHERE conname='tenant_suspension_consistent' AND conrelid='tenants'::regclass) THEN
    ALTER TABLE tenants ADD CONSTRAINT tenant_suspension_consistent CHECK((status='suspended')=(suspended_at IS NOT NULL));
  END IF;
END $$;
REVOKE UPDATE ON tenants FROM app_user;
GRANT UPDATE(name,updated_at) ON tenants TO app_user;
GRANT USAGE ON SCHEMA public TO tenant_lifecycle;
GRANT SELECT(id,status,suspended_at),UPDATE(status,suspended_at,updated_at) ON tenants TO tenant_lifecycle;
GRANT SELECT(id,tenant_id) ON identities TO tenant_lifecycle;
GRANT SELECT(identity_id,expires_at),UPDATE(expires_at) ON account_leases TO tenant_lifecycle;
GRANT SELECT(id,tenant_id,status),UPDATE(status,next_retry_at,completed_at,updated_at) ON delivery_tasks TO tenant_lifecycle;
GRANT INSERT(tenant_id,action,resource,resource_id,before,after,source) ON audit_logs TO tenant_lifecycle;
GRANT INSERT(tenant_id,level,module,event,context) ON system_logs TO tenant_lifecycle;
GRANT USAGE ON SEQUENCE audit_logs_id_seq,system_logs_id_seq TO tenant_lifecycle;
DROP POLICY IF EXISTS lifecycle_identity_keys ON identities;
CREATE POLICY lifecycle_identity_keys ON identities FOR SELECT TO tenant_lifecycle
 USING(tenant_id=current_setting('app.tenant_id')::bigint);
DROP POLICY IF EXISTS lifecycle_task_keys ON delivery_tasks;
CREATE POLICY lifecycle_task_keys ON delivery_tasks FOR SELECT TO tenant_lifecycle
 USING(tenant_id=current_setting('app.tenant_id')::bigint);
DROP POLICY IF EXISTS lifecycle_task_cancel ON delivery_tasks;
CREATE POLICY lifecycle_task_cancel ON delivery_tasks FOR UPDATE TO tenant_lifecycle
 USING(tenant_id=current_setting('app.tenant_id')::bigint AND status IN('PENDING','FAILED')
   AND EXISTS(SELECT 1 FROM tenants WHERE id=tenant_id AND status='suspended'))
 WITH CHECK(tenant_id=current_setting('app.tenant_id')::bigint AND status='CANCELLED'
   AND next_retry_at IS NULL AND completed_at IS NOT NULL);
DROP POLICY IF EXISTS lifecycle_audit ON audit_logs;
CREATE POLICY lifecycle_audit ON audit_logs FOR INSERT TO tenant_lifecycle
 WITH CHECK(tenant_id=current_setting('app.tenant_id')::bigint AND source='system'
 AND resource='tenant' AND resource_id=tenant_id
 AND ((action='tenant.suspend' AND before='{"status":"active"}'::jsonb AND after='{"status":"suspended"}'::jsonb)
 OR(action='tenant.resume' AND before='{"status":"suspended"}'::jsonb AND after='{"status":"active"}'::jsonb)));
DROP POLICY IF EXISTS lifecycle_log ON system_logs;
CREATE POLICY lifecycle_log ON system_logs FOR INSERT TO tenant_lifecycle
 WITH CHECK(tenant_id=current_setting('app.tenant_id')::bigint AND level='INFO' AND module='tenant_lifecycle' AND event='cleanup.done'
 AND jsonb_typeof(context)='object' AND context ?& ARRAY['table','rows','elapsed_ms']
 AND context-ARRAY['table','rows','elapsed_ms']='{}'::jsonb AND context->>'table'='delivery_tasks'
 AND jsonb_typeof(context->'rows')='number' AND context->>'rows' ~ '^[0-9]+$'
 AND (context->>'rows')::numeric BETWEEN 0 AND 5000
 AND jsonb_typeof(context->'elapsed_ms')='number' AND context->>'elapsed_ms' ~ '^[0-9]+$');
CREATE OR REPLACE FUNCTION lifecycle_lease_expiry() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF current_user='tenant_lifecycle' AND
   (NEW.expires_at>statement_timestamp() OR NOT EXISTS(SELECT 1 FROM identities identity JOIN tenants tenant ON tenant.id=identity.tenant_id
     WHERE identity.id=NEW.identity_id AND identity.tenant_id=current_setting('app.tenant_id')::bigint AND tenant.status='suspended')) THEN
   RAISE EXCEPTION 'invalid lease expiry' USING ERRCODE='42501';
 END IF;
 RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS lifecycle_lease_expiry ON account_leases;
CREATE TRIGGER lifecycle_lease_expiry BEFORE UPDATE ON account_leases FOR EACH ROW EXECUTE FUNCTION lifecycle_lease_expiry();
RESET ROLE;
