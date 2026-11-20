SET ROLE tgm_owner;

CREATE OR REPLACE FUNCTION tenant_suspension_time() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF current_user='tenant_lifecycle' THEN
    IF current_setting('app.tenant_id', true) IS NULL OR TG_OP<>'UPDATE'
      OR NEW.id<>current_setting('app.tenant_id', true)::bigint
      OR NOT ((OLD.status='active' AND NEW.status='suspended')
        OR (OLD.status='suspended' AND NEW.status='active')) THEN
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
END $$;

DROP TRIGGER IF EXISTS tenant_suspension_time ON tenants;
CREATE TRIGGER tenant_suspension_time BEFORE INSERT OR UPDATE OF status,suspended_at ON tenants
  FOR EACH ROW EXECUTE FUNCTION tenant_suspension_time();

CREATE OR REPLACE FUNCTION lifecycle_lease_expiry() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF current_user='tenant_lifecycle' THEN
    IF current_setting('app.tenant_id', true) IS NULL OR NEW.expires_at>statement_timestamp() OR NOT EXISTS(
      SELECT 1 FROM identities identity JOIN tenants tenant ON tenant.id=identity.tenant_id
      WHERE identity.id=NEW.identity_id
        AND identity.tenant_id=current_setting('app.tenant_id', true)::bigint
        AND tenant.status='suspended'
    ) THEN
      RAISE EXCEPTION 'invalid lease expiry' USING ERRCODE='42501';
    END IF;
  END IF;
  RETURN NEW;
END $$;

DROP TRIGGER IF EXISTS lifecycle_lease_expiry ON account_leases;
CREATE TRIGGER lifecycle_lease_expiry BEFORE UPDATE ON account_leases
  FOR EACH ROW EXECUTE FUNCTION lifecycle_lease_expiry();

RESET ROLE;
