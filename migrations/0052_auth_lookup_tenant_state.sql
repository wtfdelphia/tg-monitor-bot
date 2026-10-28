SET ROLE tgm_owner;

GRANT SELECT (id, status) ON tenants TO auth_lookup;

RESET ROLE;
