SET ROLE tgm_owner;

GRANT SELECT (tenant_id, user_id) ON public.tenant_members TO tenant_purge_batcher;
GRANT SELECT (tenant_id, fingerprint, target_ref) ON public.notify_dedup TO tenant_purge_batcher;
GRANT SELECT (tenant_id, id) ON public.tenant_notifications TO tenant_purge_batcher;
GRANT SELECT (id) ON public.rule_conditions, public.delivery_logs TO tenant_purge_batcher;
GRANT SELECT (id) ON public.login_sessions, public.web_sessions,
  public.tenant_consents, public.audit_logs, public.system_logs TO tenant_purge_batcher;
GRANT SELECT (tg_user_id) ON public.bot_console_sessions TO tenant_purge_batcher;
GRANT SELECT (bot_id, update_id) ON public.bot_console_updates TO tenant_purge_batcher;

RESET ROLE;
