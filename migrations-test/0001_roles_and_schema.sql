-- 仅用于受限 SQLx 测试库。生产角色由 scripts/init/01-roles.sql 预置。
-- 不复用生产 0001 的“把 tgm_owner 授予当前用户”逻辑，避免给 tgm_test
-- tgm_owner 的 ADMIN 权限；测试连接只保留 SET ROLE tgm_owner。
SET ROLE tgm_owner;

ALTER SCHEMA public OWNER TO tgm_owner;
GRANT USAGE ON SCHEMA public TO app_user, auth_lookup, platform_ops;

RESET ROLE;
