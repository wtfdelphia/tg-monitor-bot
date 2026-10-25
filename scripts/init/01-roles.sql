-- 定义处是 docs/design/eng/02-本地环境与迁移.md §二，本文件是它的可执行形式。
-- 权限矩阵在同文 §二 的表格里，逐权限列举、不写 GRANT ALL —— 理由是 TRUNCATE 不受 RLS 约束。
CREATE ROLE tgm_owner   NOLOGIN;                                -- 建表与迁移
CREATE ROLE app_user    LOGIN PASSWORD 'apw' NOBYPASSRLS;       -- 业务运行时
CREATE ROLE auth_lookup LOGIN PASSWORD 'lpw' NOBYPASSRLS;       -- 鉴权反查专用
CREATE ROLE platform_ops LOGIN PASSWORD 'ppw' NOSUPERUSER NOBYPASSRLS; -- 平台日志只读
CREATE ROLE auth_cleanup LOGIN PASSWORD 'cpw' NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
CREATE ROLE tenant_cleanup LOGIN PASSWORD 'tcpw' NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
CREATE ROLE tgm_test    LOGIN PASSWORD 'tpw' NOSUPERUSER CREATEDB NOCREATEROLE NOINHERIT NOBYPASSRLS;
GRANT tgm_owner TO postgres;
GRANT tgm_owner TO tgm_test WITH SET TRUE, INHERIT FALSE;

-- 坑 1：PG 15+ 起 public schema 默认不对非 owner 开放。
-- 漏这一行，迁移第一条 CREATE TABLE 就 "permission denied for schema public"
ALTER SCHEMA public OWNER TO tgm_owner;

GRANT USAGE ON SCHEMA public TO app_user, auth_lookup, platform_ops;

-- 测试角色只用于 SQLx 控制库、临时库和 schema 库，不得连接生产库。
REVOKE CONNECT ON DATABASE tgm FROM PUBLIC;
GRANT CONNECT ON DATABASE tgm TO postgres, app_user, auth_lookup, platform_ops;

-- SQLx 的控制连接不能落到生产库；schema 库单独供 cargo sqlx prepare 使用。
\connect postgres
SELECT format('CREATE DATABASE tgm_test_control OWNER tgm_test')
WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'tgm_test_control')\gexec
SELECT format('CREATE DATABASE tgm_test_schema OWNER tgm_test')
WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'tgm_test_schema')\gexec

-- SQLx 创建临时库时默认克隆 template1。预置与 0013 同形的触发器，
-- 测试迁移就不需要让受限角色创建 event trigger。
\connect template1
-- SQLx 临时库由 tgm_test 创建。保留 public 的 owner 为 tgm_owner，
-- 只授予 tgm_test 创建 SQLx 迁移账本的权限；生产迁移仍由 tgm_owner 建表。
ALTER SCHEMA public OWNER TO tgm_owner;
GRANT USAGE, CREATE ON SCHEMA public TO tgm_test;
SET ROLE tgm_owner;
CREATE OR REPLACE FUNCTION rls_auto_enable() RETURNS event_trigger
LANGUAGE plpgsql
AS $$
DECLARE cmd record;
BEGIN
  FOR cmd IN SELECT * FROM pg_event_trigger_ddl_commands()
             WHERE object_type IN ('table', 'partitioned table')
  LOOP
    CONTINUE WHEN (SELECT relpersistence FROM pg_class WHERE oid = cmd.objid) = 't'
      OR (SELECT relname FROM pg_class WHERE oid = cmd.objid) = '_sqlx_migrations'
      OR (SELECT nspname
            FROM pg_namespace
           WHERE oid = (SELECT relnamespace FROM pg_class WHERE oid = cmd.objid))
         = '_sqlx_test';
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', cmd.object_identity);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', cmd.object_identity);
  END LOOP;
END
$$;
RESET ROLE;
DROP EVENT TRIGGER IF EXISTS ensure_rls;
CREATE EVENT TRIGGER ensure_rls ON ddl_command_end
  WHEN TAG IN ('CREATE TABLE', 'CREATE TABLE AS') EXECUTE FUNCTION rls_auto_enable();
