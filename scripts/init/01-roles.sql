-- 定义处是 docs/design/eng/02-本地环境与迁移.md §二，本文件是它的可执行形式。
-- 权限矩阵在同文 §二 的表格里，逐权限列举、不写 GRANT ALL —— 理由是 TRUNCATE 不受 RLS 约束。
CREATE ROLE tgm_owner   NOLOGIN;                                -- 建表与迁移
CREATE ROLE app_user    LOGIN PASSWORD 'apw' NOBYPASSRLS;       -- 业务运行时
CREATE ROLE auth_lookup LOGIN PASSWORD 'lpw' NOBYPASSRLS;       -- 鉴权反查专用
CREATE ROLE platform_ops LOGIN PASSWORD 'ppw' NOSUPERUSER NOBYPASSRLS; -- 平台日志只读
GRANT tgm_owner TO postgres;

-- 坑 1：PG 15+ 起 public schema 默认不对非 owner 开放。
-- 漏这一行，迁移第一条 CREATE TABLE 就 "permission denied for schema public"
ALTER SCHEMA public OWNER TO tgm_owner;

GRANT USAGE ON SCHEMA public TO app_user, auth_lookup, platform_ops;
