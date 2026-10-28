SET ROLE tgm_owner;

CREATE OR REPLACE FUNCTION public.identity_lifecycle_write_fence()
RETURNS trigger LANGUAGE plpgsql VOLATILE SECURITY INVOKER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
  IF current_user = 'app_user' AND NEW.tenant_id IS NOT NULL THEN
    IF current_setting('transaction_isolation') <> 'read committed'
      OR NULLIF(current_setting('app.tenant_id', true), '') IS DISTINCT FROM NEW.tenant_id::text
    THEN
      RAISE EXCEPTION 'invalid identity lifecycle write' USING ERRCODE = '42501';
    END IF;
    PERFORM pg_catalog.pg_advisory_xact_lock_shared(
      pg_catalog.hashtextextended('tenant_lifecycle:' || NEW.tenant_id::text, 419871)
    );
    IF NOT EXISTS (
      SELECT 1 FROM public.tenants WHERE id = NEW.tenant_id AND status = 'active'
    ) THEN
      RAISE EXCEPTION 'invalid identity lifecycle write' USING ERRCODE = '42501';
    END IF;
  END IF;
  RETURN NEW;
END
$$;

REVOKE ALL ON FUNCTION public.identity_lifecycle_write_fence() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.identity_lifecycle_write_fence() TO app_user;

DROP TRIGGER IF EXISTS identity_lifecycle_write_fence ON public.identities;
CREATE TRIGGER identity_lifecycle_write_fence
BEFORE INSERT OR UPDATE OF tenant_id ON public.identities
FOR EACH ROW EXECUTE FUNCTION public.identity_lifecycle_write_fence();

RESET ROLE;
