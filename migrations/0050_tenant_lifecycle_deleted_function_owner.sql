RESET ROLE;

ALTER FUNCTION tenant_lifecycle_tenant_is_deleted(BIGINT) OWNER TO postgres;

SET ROLE tgm_owner;

RESET ROLE;
