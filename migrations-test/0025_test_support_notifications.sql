-- 仅用于受限 SQLx 测试库。不得复制进生产 migrations/。
-- 0014 只覆盖它运行时已存在的表；0024 新建的 tenant_notifications 照同一形状补给 tgm_test。
SET ROLE tgm_owner;

GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE tenant_notifications TO tgm_test;
GRANT USAGE, SELECT, UPDATE ON SEQUENCE tenant_notifications_id_seq TO tgm_test;
DROP POLICY IF EXISTS tgm_test_admin ON tenant_notifications;
CREATE POLICY tgm_test_admin ON tenant_notifications TO tgm_test USING (true) WITH CHECK (true);

RESET ROLE;
