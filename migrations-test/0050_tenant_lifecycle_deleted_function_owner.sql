SET ROLE tgm_owner;

ALTER FUNCTION tenant_lifecycle_tenant_is_deleted(BIGINT) OWNER TO tgm_owner;

RESET ROLE;
