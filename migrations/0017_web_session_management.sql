SET ROLE tgm_owner;

ALTER TABLE web_sessions ADD COLUMN IF NOT EXISTS login_method TEXT NOT NULL DEFAULT 'unknown'
  CONSTRAINT web_sessions_login_method_check CHECK (login_method IN ('oidc', 'bot_qr', 'unknown'));

DROP POLICY IF EXISTS session_self_lookup ON web_sessions;
CREATE POLICY session_self_lookup ON web_sessions FOR SELECT TO auth_lookup
  USING (id = NULLIF(current_setting('app.session_id', true), '')::bytea);

DROP POLICY IF EXISTS session_self_delete ON web_sessions;
CREATE POLICY session_self_delete ON web_sessions FOR DELETE TO auth_lookup
  USING (id = NULLIF(current_setting('app.session_id', true), '')::bytea);

DROP POLICY IF EXISTS session_owner_list ON web_sessions;
CREATE POLICY session_owner_list ON web_sessions FOR SELECT TO auth_lookup
  USING (user_id = NULLIF(current_setting('app.auth_user_id', true), '')::bigint);

DROP POLICY IF EXISTS session_owner_delete ON web_sessions;
CREATE POLICY session_owner_delete ON web_sessions FOR DELETE TO auth_lookup
  USING (user_id = NULLIF(current_setting('app.auth_user_id', true), '')::bigint);

RESET ROLE;
