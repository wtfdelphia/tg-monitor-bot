DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tenant_purge') THEN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = current_user AND (rolsuper OR rolcreaterole)) THEN
      RAISE EXCEPTION 'tenant_purge role must be pre-provisioned';
    END IF;
    CREATE ROLE tenant_purge LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tenant_purge_finalizer') THEN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = current_user AND (rolsuper OR rolcreaterole)) THEN
      RAISE EXCEPTION 'tenant_purge_finalizer role must be pre-provisioned';
    END IF;
    CREATE ROLE tenant_purge_finalizer NOLOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
  END IF;
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO tenant_purge', current_database());
END
$$;

GRANT tenant_purge_finalizer TO tgm_owner WITH ADMIN OPTION;
SET ROLE tgm_owner;

GRANT USAGE ON SCHEMA public TO tenant_purge, tenant_purge_finalizer;
REVOKE CREATE ON SCHEMA public FROM tenant_purge;
GRANT CREATE ON SCHEMA public TO tenant_purge_finalizer;

GRANT DELETE ON public.tenants TO tenant_purge_finalizer;
GRANT SELECT (id, status, deleted_at) ON public.tenants TO tenant_purge_finalizer;
GRANT SELECT (id, tenant_id) ON public.identities TO tenant_purge_finalizer;
GRANT SELECT (tenant_id) ON public.tenant_members,
  public.credential_keys,
  public.keywords,
  public.rule_versions,
  public.rule_replacements,
  public.notify_dedup,
  public.audit_logs,
  public.system_logs,
  public.login_sessions,
  public.bot_console_sessions,
  public.bot_console_updates,
  public.web_sessions,
  public.tenant_consents TO tenant_purge_finalizer;
GRANT SELECT (tenant_id, identity_id) ON public.tenant_notifications TO tenant_purge_finalizer;
GRANT SELECT (tenant_id, bot_id) ON public.bot_console_updates TO tenant_purge_finalizer;
GRANT SELECT (tenant_id, receiver_identity_id) ON public.source_subscriptions TO tenant_purge_finalizer;
GRANT SELECT (tenant_id, sender_identity_id) ON public.target_channels TO tenant_purge_finalizer;
GRANT SELECT (tenant_id, id, sender_identity_id) ON public.delivery_tasks TO tenant_purge_finalizer;
GRANT SELECT (task_id) ON public.delivery_logs TO tenant_purge_finalizer;
GRANT SELECT (tenant_id, id) ON public.rules TO tenant_purge_finalizer;
GRANT SELECT (rule_id) ON public.rule_conditions TO tenant_purge_finalizer;
GRANT SELECT (identity_id) ON public.identity_secrets, public.account_leases TO tenant_purge_finalizer;
GRANT SELECT (id, receiver_identity_id) ON public.canonical_events TO tenant_purge_finalizer;
GRANT SELECT (tenant_id, canonical_event_id) ON public.tenant_events TO tenant_purge_finalizer;
GRANT SELECT (group_event_id, member_event_id) ON public.media_group_members TO tenant_purge_finalizer;
GRANT SELECT (canonical_event_id, receiver_identity_id, status) ON public.media_relay_jobs TO tenant_purge_finalizer;
GRANT INSERT (tenant_id, level, module, event, context) ON public.system_logs TO tenant_purge_finalizer;
GRANT USAGE ON SEQUENCE public.system_logs_id_seq TO tenant_purge_finalizer;

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.identities;
CREATE POLICY tenant_purge_finalizer_read ON public.identities
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.tenant_members;
CREATE POLICY tenant_purge_finalizer_read ON public.tenant_members
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.credential_keys;
CREATE POLICY tenant_purge_finalizer_read ON public.credential_keys
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.source_subscriptions;
CREATE POLICY tenant_purge_finalizer_read ON public.source_subscriptions
  FOR SELECT TO tenant_purge_finalizer
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR receiver_identity_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.target_channels;
CREATE POLICY tenant_purge_finalizer_read ON public.target_channels
  FOR SELECT TO tenant_purge_finalizer
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR sender_identity_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.keywords;
CREATE POLICY tenant_purge_finalizer_read ON public.keywords
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.rules;
CREATE POLICY tenant_purge_finalizer_read ON public.rules
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.rule_versions;
CREATE POLICY tenant_purge_finalizer_read ON public.rule_versions
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.rule_replacements;
CREATE POLICY tenant_purge_finalizer_read ON public.rule_replacements
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.tenant_events;
CREATE POLICY tenant_purge_finalizer_read ON public.tenant_events
  FOR SELECT TO tenant_purge_finalizer
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR canonical_event_id IN (
      SELECT id FROM public.canonical_events
       WHERE receiver_identity_id IN (
         SELECT id FROM public.identities
          WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
       )
    )
  );

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.notify_dedup;
CREATE POLICY tenant_purge_finalizer_read ON public.notify_dedup
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.delivery_tasks;
CREATE POLICY tenant_purge_finalizer_read ON public.delivery_tasks
  FOR SELECT TO tenant_purge_finalizer
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR sender_identity_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.audit_logs;
CREATE POLICY tenant_purge_finalizer_read ON public.audit_logs
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.system_logs;
CREATE POLICY tenant_purge_finalizer_read ON public.system_logs
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.login_sessions;
CREATE POLICY tenant_purge_finalizer_read ON public.login_sessions
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.bot_console_sessions;
CREATE POLICY tenant_purge_finalizer_read ON public.bot_console_sessions
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.bot_console_updates;
CREATE POLICY tenant_purge_finalizer_read ON public.bot_console_updates
  FOR SELECT TO tenant_purge_finalizer
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR bot_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.web_sessions;
CREATE POLICY tenant_purge_finalizer_read ON public.web_sessions
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.tenant_consents;
CREATE POLICY tenant_purge_finalizer_read ON public.tenant_consents
  FOR SELECT TO tenant_purge_finalizer
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);

DROP POLICY IF EXISTS tenant_purge_finalizer_read ON public.tenant_notifications;
CREATE POLICY tenant_purge_finalizer_read ON public.tenant_notifications
  FOR SELECT TO tenant_purge_finalizer
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR identity_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );

DROP POLICY IF EXISTS tenant_purge_finalizer_log ON public.system_logs;
CREATE POLICY tenant_purge_finalizer_log ON public.system_logs
  FOR INSERT TO tenant_purge_finalizer
  WITH CHECK (
    tenant_id IS NULL
    AND level = 'INFO'
    AND module = 'tenant_purge'
    AND event = 'cleanup.done'
    AND jsonb_typeof(context) = 'object'
    AND context ?& ARRAY['tenant_id', 'category', 'result', 'rows']
    AND context - ARRAY['tenant_id', 'category', 'result', 'rows'] = '{}'::jsonb
    AND jsonb_typeof(context->'tenant_id') = 'number'
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' = '1'
    AND context->>'category' = 'tenant_root'
    AND context->>'result' = 'deleted'
  );

RESET ROLE;
SET ROLE tenant_purge_finalizer;

CREATE OR REPLACE FUNCTION public.tenant_purge_finalize(tenant_key bigint)
RETURNS text
LANGUAGE plpgsql
SECURITY DEFINER
VOLATILE
PARALLEL UNSAFE
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
  IF session_user <> 'tenant_purge'
    OR current_user <> 'tenant_purge_finalizer'
    OR current_setting('transaction_isolation') <> 'read committed'
  THEN
    RAISE EXCEPTION 'invalid tenant purge context' USING ERRCODE = '42501';
  END IF;

  IF NOT pg_catalog.pg_try_advisory_xact_lock(
    pg_catalog.hashtextextended('tenant_lifecycle:' || tenant_key::text, 419871)
  ) THEN
    RETURN 'locked';
  END IF;

  PERFORM pg_catalog.set_config('app.purge_tenant_id', tenant_key::text, true);

  IF NOT EXISTS (
    SELECT 1 FROM public.tenants
     WHERE id = tenant_key AND status = 'deleted'
  ) THEN
    RETURN 'not_deleted';
  END IF;

  IF NOT EXISTS (
    SELECT 1 FROM public.tenants
     WHERE id = tenant_key
       AND deleted_at IS NOT NULL
       AND deleted_at <= pg_catalog.statement_timestamp() - pg_catalog.interval '30 days'
  ) THEN
    RETURN 'not_due';
  END IF;

  IF EXISTS (SELECT 1 FROM public.tenant_members WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.credential_keys WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.source_subscriptions WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.target_channels WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.keywords WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.rules WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.rule_versions WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.rule_replacements WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.tenant_events WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.notify_dedup WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.delivery_tasks WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.audit_logs WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.system_logs WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.login_sessions WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.bot_console_sessions WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.bot_console_updates WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.web_sessions WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.tenant_consents WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.tenant_notifications WHERE tenant_id = tenant_key)
    OR EXISTS (SELECT 1 FROM public.identities WHERE tenant_id = tenant_key)
  THEN
    RETURN 'blocked';
  END IF;

  IF EXISTS (
    SELECT 1 FROM public.source_subscriptions
     WHERE tenant_id <> tenant_key
       AND receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
  ) OR EXISTS (
    SELECT 1 FROM public.target_channels
     WHERE tenant_id <> tenant_key
       AND sender_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
  ) OR EXISTS (
    SELECT 1 FROM public.delivery_tasks
     WHERE tenant_id <> tenant_key
       AND sender_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
  ) OR EXISTS (
    SELECT 1 FROM public.canonical_events
     WHERE receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
  ) OR EXISTS (
    SELECT 1 FROM public.bot_console_updates
     WHERE tenant_id <> tenant_key
       AND bot_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
  ) OR EXISTS (
    SELECT 1 FROM public.tenant_notifications
     WHERE tenant_id <> tenant_key
       AND identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
  ) OR EXISTS (
    SELECT 1 FROM public.media_relay_jobs
     WHERE (
       receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
       OR canonical_event_id IN (
         SELECT id FROM public.canonical_events
          WHERE receiver_identity_id IN (
            SELECT id FROM public.identities WHERE tenant_id = tenant_key
          )
       )
     )
       AND status IN ('PENDING', 'DOWNLOADING', 'READY')
  ) OR EXISTS (
    SELECT 1 FROM public.media_group_members
     WHERE group_event_id IN (
       SELECT id FROM public.canonical_events
        WHERE receiver_identity_id IN (
          SELECT id FROM public.identities WHERE tenant_id = tenant_key
        )
     )
       OR member_event_id IN (
         SELECT id FROM public.canonical_events
          WHERE receiver_identity_id IN (
            SELECT id FROM public.identities WHERE tenant_id = tenant_key
          )
       )
  )
  THEN
    RETURN 'blocked';
  END IF;

  DELETE FROM public.tenants WHERE id = tenant_key;
  IF NOT FOUND THEN
    RETURN 'not_found';
  END IF;

  INSERT INTO public.system_logs (tenant_id, level, module, event, context)
  VALUES (
    NULL,
    'INFO',
    'tenant_purge',
    'cleanup.done',
    pg_catalog.jsonb_build_object(
      'tenant_id', tenant_key,
      'category', 'tenant_root',
      'result', 'deleted',
      'rows', 1
    )
  );

  RETURN 'deleted';
END
$$;

ALTER FUNCTION public.tenant_purge_finalize(bigint) OWNER TO tenant_purge_finalizer;
REVOKE ALL ON FUNCTION public.tenant_purge_finalize(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.tenant_purge_finalize(bigint) TO tenant_purge;
RESET ROLE;
SET ROLE tgm_owner;
REVOKE CREATE ON SCHEMA public FROM tenant_purge_finalizer;
RESET ROLE;
REVOKE tenant_purge_finalizer FROM tgm_owner;
