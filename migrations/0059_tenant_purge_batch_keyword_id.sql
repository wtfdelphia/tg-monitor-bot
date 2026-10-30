SET ROLE tgm_owner;
GRANT SELECT (id) ON public.keywords TO tenant_purge_batcher;
RESET ROLE;
