SET ROLE tgm_owner;
GRANT CREATE ON SCHEMA public TO tenant_purge_batcher;
RESET ROLE;
SET ROLE tenant_purge_batcher;

CREATE OR REPLACE FUNCTION public.tenant_purge_candidates(candidate_limit bigint)
RETURNS SETOF bigint
LANGUAGE plpgsql
SECURITY DEFINER
VOLATILE
PARALLEL UNSAFE
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
  IF session_user <> 'tenant_purge'
    OR current_user <> 'tenant_purge_batcher'
    OR current_setting('transaction_isolation') <> 'read committed'
    OR candidate_limit < 1
    OR candidate_limit > 5000
  THEN
    RAISE EXCEPTION 'invalid tenant purge candidate context' USING ERRCODE = '42501';
  END IF;

  RETURN QUERY
  SELECT tenants.id
    FROM public.tenants
   WHERE tenants.status = 'deleted'
     AND tenants.deleted_at IS NOT NULL
     AND tenants.deleted_at <= pg_catalog.statement_timestamp() - pg_catalog.interval '30 days'
   ORDER BY tenants.id
   LIMIT candidate_limit;
END
$$;

ALTER FUNCTION public.tenant_purge_candidates(bigint) OWNER TO tenant_purge_batcher;
REVOKE ALL ON FUNCTION public.tenant_purge_candidates(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.tenant_purge_candidates(bigint) TO tenant_purge;
RESET ROLE;
SET ROLE tgm_owner;
REVOKE CREATE ON SCHEMA public FROM tenant_purge_batcher;
RESET ROLE;
