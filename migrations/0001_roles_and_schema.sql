-- 角色与 schema owner。定义处是 docs/design/eng/02-本地环境与迁移.md §二。
--
-- 可重入（ADR-0021：不写 down 迁移，代价是每个文件必须能连跑两遍）。
-- PG 没有 CREATE ROLE IF NOT EXISTS，故用 DO 块查 pg_roles。
--
-- 偏离 eng/02 §二：本文件不设口令。
--   §二 的口令（apw/lpw/ppw）是本地开发值，写在 scripts/init/01-roles.sql 里，
--   由 compose 的 docker-entrypoint-initdb.d 先跑、本文件后跑，届时角色已存在故跳过。
--   迁移会在生产上执行，把开发口令写进来等于把它带上生产。
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tgm_owner') THEN
    CREATE ROLE tgm_owner NOLOGIN;                       -- 建表与迁移
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_user') THEN
    CREATE ROLE app_user LOGIN NOBYPASSRLS;              -- 业务运行时
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'auth_lookup') THEN
    CREATE ROLE auth_lookup LOGIN NOBYPASSRLS;           -- 鉴权反查专用
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'platform_ops') THEN
    CREATE ROLE platform_ops LOGIN NOSUPERUSER NOBYPASSRLS;  -- 平台日志只读
  END IF;
END $$;

-- 迁移执行者需要 tgm_owner 才能建表。GRANT 是幂等的。
DO $$
BEGIN
  EXECUTE format('GRANT tgm_owner TO %I', current_user);
END $$;

-- 坑 1（eng/02 §零）：PG 15+ 起 public schema 默认不对非 owner 开放。
-- 漏这一行，下一个文件第一条 CREATE TABLE 就 "permission denied for schema public"。
ALTER SCHEMA public OWNER TO tgm_owner;

GRANT USAGE ON SCHEMA public TO app_user, auth_lookup, platform_ops;
