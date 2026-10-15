-- 控制台扫码确认后的 web_sessions 写入路径。
-- 这是新增迁移，不修改已应用的 0010（ADR-0021：迁移只增不改）。
SET ROLE tgm_owner;

DROP POLICY IF EXISTS session_self_insert ON web_sessions;
CREATE POLICY session_self_insert ON web_sessions FOR INSERT TO auth_lookup
  WITH CHECK (user_id = current_setting('app.auth_user_id')::bigint);

RESET ROLE;
