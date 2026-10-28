SET ROLE tgm_owner;

GRANT SELECT (identity_id) ON identity_secrets TO tenant_lifecycle;

RESET ROLE;
