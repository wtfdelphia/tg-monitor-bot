-- 当前控制台会话登出路径。
-- 这是新增迁移，不修改已应用的 0010（ADR-0021：迁移只增不改）。
SET ROLE tgm_owner;

DROP POLICY IF EXISTS session_self_delete ON web_sessions;
CREATE POLICY session_self_delete ON web_sessions FOR DELETE TO auth_lookup
  USING (id = current_setting('app.session_id')::bytea);

RESET ROLE;
