-- 仅用于受限 SQLx 测试库。不得复制进生产 migrations/。
SET ROLE tgm_owner;

GRANT USAGE, CREATE ON SCHEMA public TO tgm_test;
GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO tgm_test;

-- 模板库预置的兜底触发器会早于生产迁移创建表，因此也会碰到
-- 0009 明确列出的六张“刻意不启用 RLS”表。恢复生产基线，避免
-- 没有策略的 auth_lookup 在测试库里被静默挡成空集。
ALTER TABLE tenants DISABLE ROW LEVEL SECURITY;
ALTER TABLE canonical_events DISABLE ROW LEVEL SECURITY;
ALTER TABLE media_archive DISABLE ROW LEVEL SECURITY;
ALTER TABLE identity_secrets DISABLE ROW LEVEL SECURITY;
ALTER TABLE account_leases DISABLE ROW LEVEL SECURITY;
ALTER TABLE console_login_challenges DISABLE ROW LEVEL SECURITY;

DO $$
DECLARE
  table_name text;
BEGIN
  FOR table_name IN
    SELECT c.relname
      FROM pg_class c
      JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public'
       AND c.relkind IN ('r', 'p')
       AND c.relname <> '_sqlx_migrations'
  LOOP
    EXECUTE format(
      'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE %I TO tgm_test',
      table_name
    );
    IF (SELECT c.relrowsecurity
          FROM pg_class c
          JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' AND c.relname = table_name) THEN
      EXECUTE format('DROP POLICY IF EXISTS tgm_test_admin ON %I', table_name);
      EXECUTE format(
        'CREATE POLICY tgm_test_admin ON %I TO tgm_test USING (true) WITH CHECK (true)',
        table_name
      );
    END IF;
  END LOOP;
END
$$;

RESET ROLE;
