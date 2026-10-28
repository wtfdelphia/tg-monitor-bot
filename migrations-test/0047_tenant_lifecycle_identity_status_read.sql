SET ROLE tgm_owner;

GRANT SELECT (status) ON identities TO tenant_lifecycle;

RESET ROLE;
