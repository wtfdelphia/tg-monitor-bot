SET ROLE tgm_owner;

GRANT SELECT (source_ref) ON public.tenant_events TO tenant_purge_batcher;
GRANT SELECT (target_ref) ON public.rules TO tenant_purge_batcher;

RESET ROLE;
