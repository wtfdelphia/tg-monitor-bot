SET ROLE tgm_owner;

REVOKE SELECT (id) ON web_sessions FROM tenant_lifecycle;
REVOKE SELECT (id) ON login_sessions FROM tenant_lifecycle;
REVOKE SELECT (tg_user_id) ON bot_console_sessions FROM tenant_lifecycle;

RESET ROLE;
