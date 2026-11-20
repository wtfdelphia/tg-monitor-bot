DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tenant_purge') THEN
    IF NOT EXISTS (
      SELECT 1 FROM pg_roles
       WHERE rolname = current_user AND (rolsuper OR rolcreaterole)
    ) THEN
      RAISE EXCEPTION 'tenant_purge role must be pre-provisioned';
    END IF;
    CREATE ROLE tenant_purge LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tenant_purge_batcher') THEN
    IF NOT EXISTS (
      SELECT 1 FROM pg_roles
       WHERE rolname = current_user AND (rolsuper OR rolcreaterole)
    ) THEN
      RAISE EXCEPTION 'tenant_purge_batcher role must be pre-provisioned';
    END IF;
    CREATE ROLE tenant_purge_batcher NOLOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION;
  END IF;
  EXECUTE format('GRANT CONNECT ON DATABASE %I TO tenant_purge', current_database());
END
$$;

GRANT tenant_purge_batcher TO tgm_owner WITH ADMIN OPTION;
SET ROLE tgm_owner;

GRANT USAGE ON SCHEMA public TO tenant_purge, tenant_purge_batcher;
REVOKE CREATE ON SCHEMA public FROM tenant_purge, tenant_purge_batcher;

GRANT SELECT (id, status, deleted_at) ON public.tenants TO tenant_purge_batcher;
GRANT SELECT (id, tenant_id) ON public.identities TO tenant_purge_batcher;
GRANT SELECT (tenant_id, id) ON public.rules TO tenant_purge_batcher;
GRANT SELECT (tenant_id, rule_id, version) ON public.rule_versions TO tenant_purge_batcher;
GRANT SELECT (rule_id) ON public.rule_conditions TO tenant_purge_batcher;
GRANT SELECT (tenant_id, rule_id, seq) ON public.rule_replacements TO tenant_purge_batcher;
GRANT SELECT (tenant_id, id) ON public.source_subscriptions TO tenant_purge_batcher;
GRANT SELECT (tenant_id, id) ON public.target_channels TO tenant_purge_batcher;
GRANT SELECT (tenant_id) ON public.keywords TO tenant_purge_batcher;
GRANT SELECT (tenant_id) ON public.tenant_members, public.credential_keys,
  public.login_sessions, public.bot_console_sessions, public.bot_console_updates,
  public.web_sessions, public.tenant_consents, public.notify_dedup,
  public.tenant_notifications, public.audit_logs, public.system_logs
  TO tenant_purge_batcher;
GRANT SELECT (user_id) ON public.tenant_members TO tenant_purge_batcher;
GRANT SELECT (id) ON public.tenant_consents, public.tenant_notifications, public.system_logs
  TO tenant_purge_batcher;
GRANT SELECT (id) ON public.web_sessions, public.login_sessions TO tenant_purge_batcher;
GRANT SELECT (tg_user_id) ON public.bot_console_sessions TO tenant_purge_batcher;
GRANT SELECT (tenant_id, id, sender_identity_id, tenant_event_id, status, updated_at)
  ON public.delivery_tasks TO tenant_purge_batcher;
GRANT SELECT (task_id) ON public.delivery_logs TO tenant_purge_batcher;
GRANT SELECT (tenant_id, id, canonical_event_id) ON public.tenant_events TO tenant_purge_batcher;
GRANT SELECT (identity_id) ON public.identity_secrets, public.account_leases TO tenant_purge_batcher;
GRANT SELECT (canonical_event_id, receiver_identity_id, status)
  ON public.media_relay_jobs TO tenant_purge_batcher;
GRANT SELECT (id, receiver_identity_id) ON public.canonical_events TO tenant_purge_batcher;
GRANT SELECT (group_event_id, member_event_id) ON public.media_group_members TO tenant_purge_batcher;
GRANT DELETE ON public.tenant_members, public.credential_keys,
  public.source_subscriptions, public.target_channels, public.keywords,
  public.rules, public.rule_versions, public.rule_replacements,
  public.rule_conditions, public.tenant_events, public.notify_dedup,
  public.delivery_tasks, public.delivery_logs, public.login_sessions,
  public.bot_console_sessions, public.bot_console_updates, public.web_sessions,
  public.tenant_consents, public.tenant_notifications, public.audit_logs,
  public.system_logs
  TO tenant_purge_batcher;
GRANT DELETE ON public.identity_secrets, public.account_leases TO tenant_purge_batcher;
GRANT INSERT (tenant_id, level, module, event, context) ON public.system_logs TO tenant_purge_batcher;
GRANT USAGE ON SEQUENCE public.system_logs_id_seq TO tenant_purge_batcher;

DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.identities;
CREATE POLICY tenant_purge_batcher_read ON public.identities
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.tenant_members;
CREATE POLICY tenant_purge_batcher_read ON public.tenant_members
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.credential_keys;
CREATE POLICY tenant_purge_batcher_read ON public.credential_keys
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.source_subscriptions;
CREATE POLICY tenant_purge_batcher_read ON public.source_subscriptions
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.target_channels;
CREATE POLICY tenant_purge_batcher_read ON public.target_channels
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.keywords;
CREATE POLICY tenant_purge_batcher_read ON public.keywords
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.rules;
CREATE POLICY tenant_purge_batcher_read ON public.rules
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.rule_versions;
CREATE POLICY tenant_purge_batcher_read ON public.rule_versions
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.rule_replacements;
CREATE POLICY tenant_purge_batcher_read ON public.rule_replacements
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.tenant_events;
CREATE POLICY tenant_purge_batcher_read ON public.tenant_events
  FOR SELECT TO tenant_purge_batcher
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
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.rule_conditions;
CREATE POLICY tenant_purge_batcher_read ON public.rule_conditions
  FOR SELECT TO tenant_purge_batcher
  USING (EXISTS (
    SELECT 1 FROM public.rules parent
     WHERE parent.id = rule_conditions.rule_id
       AND parent.tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
  ));
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.delivery_logs;
CREATE POLICY tenant_purge_batcher_read ON public.delivery_logs
  FOR SELECT TO tenant_purge_batcher
  USING (EXISTS (
    SELECT 1 FROM public.delivery_tasks parent
     WHERE parent.id = delivery_logs.task_id
       AND parent.tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
  ));
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.tenant_events;
CREATE POLICY tenant_purge_batcher_delete ON public.tenant_events
  FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.notify_dedup;
CREATE POLICY tenant_purge_batcher_read ON public.notify_dedup
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.delivery_tasks;
CREATE POLICY tenant_purge_batcher_read ON public.delivery_tasks
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.audit_logs;
CREATE POLICY tenant_purge_batcher_read ON public.audit_logs
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.system_logs;
CREATE POLICY tenant_purge_batcher_read ON public.system_logs
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.login_sessions;
CREATE POLICY tenant_purge_batcher_read ON public.login_sessions
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.bot_console_sessions;
CREATE POLICY tenant_purge_batcher_read ON public.bot_console_sessions
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.bot_console_updates;
CREATE POLICY tenant_purge_batcher_read ON public.bot_console_updates
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.web_sessions;
CREATE POLICY tenant_purge_batcher_read ON public.web_sessions
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.tenant_consents;
CREATE POLICY tenant_purge_batcher_read ON public.tenant_consents
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.tenant_notifications;
CREATE POLICY tenant_purge_batcher_read ON public.tenant_notifications
  FOR SELECT TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);


DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.tenant_members;
CREATE POLICY tenant_purge_batcher_delete ON public.tenant_members FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.credential_keys;
CREATE POLICY tenant_purge_batcher_delete ON public.credential_keys FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.source_subscriptions;
CREATE POLICY tenant_purge_batcher_delete ON public.source_subscriptions FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.target_channels;
CREATE POLICY tenant_purge_batcher_delete ON public.target_channels FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.keywords;
CREATE POLICY tenant_purge_batcher_delete ON public.keywords FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.rules;
CREATE POLICY tenant_purge_batcher_delete ON public.rules FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.rule_versions;
CREATE POLICY tenant_purge_batcher_delete ON public.rule_versions FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.rule_replacements;
CREATE POLICY tenant_purge_batcher_delete ON public.rule_replacements FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.rule_conditions;
CREATE POLICY tenant_purge_batcher_delete ON public.rule_conditions FOR DELETE TO tenant_purge_batcher
  USING (EXISTS (SELECT 1 FROM public.rules parent WHERE parent.id = rule_conditions.rule_id AND parent.tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint));
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.notify_dedup;
CREATE POLICY tenant_purge_batcher_delete ON public.notify_dedup FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.delivery_tasks;
CREATE POLICY tenant_purge_batcher_delete ON public.delivery_tasks FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.delivery_logs;
CREATE POLICY tenant_purge_batcher_delete ON public.delivery_logs FOR DELETE TO tenant_purge_batcher
  USING (EXISTS (SELECT 1 FROM public.delivery_tasks parent WHERE parent.id = delivery_logs.task_id AND parent.tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint));
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.login_sessions;
CREATE POLICY tenant_purge_batcher_delete ON public.login_sessions FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.bot_console_sessions;
CREATE POLICY tenant_purge_batcher_delete ON public.bot_console_sessions FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.bot_console_updates;
CREATE POLICY tenant_purge_batcher_delete ON public.bot_console_updates FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.web_sessions;
CREATE POLICY tenant_purge_batcher_delete ON public.web_sessions FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.tenant_consents;
CREATE POLICY tenant_purge_batcher_delete ON public.tenant_consents FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.tenant_notifications;
CREATE POLICY tenant_purge_batcher_delete ON public.tenant_notifications FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.audit_logs;
CREATE POLICY tenant_purge_batcher_delete ON public.audit_logs FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_delete ON public.system_logs;
CREATE POLICY tenant_purge_batcher_delete ON public.system_logs FOR DELETE TO tenant_purge_batcher
  USING (tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint);
DROP POLICY IF EXISTS tenant_purge_batcher_log ON public.system_logs;
CREATE POLICY tenant_purge_batcher_log ON public.system_logs
  FOR INSERT TO tenant_purge_batcher
  WITH CHECK (
    tenant_id IS NULL
    AND level = 'INFO'
    AND module = 'tenant_purge'
    AND event = 'cleanup.done'
    AND jsonb_typeof(context) = 'object'
    AND context ?& ARRAY['tenant_id', 'category', 'result', 'rows', 'blocked']
    AND context - ARRAY['tenant_id', 'category', 'result', 'rows', 'blocked'] = '{}'::jsonb
    AND jsonb_typeof(context->'tenant_id') = 'number'
    AND jsonb_typeof(context->'rows') = 'number'
    AND context->>'rows' ~ '^[0-9]+$'
    AND (context->>'rows')::numeric BETWEEN 0 AND 5000
    AND context->>'category' IN (
      'identity_dependents', 'rule_dependents', 'configuration',
      'notifications', 'terminal_delivery', 'tenant_events', 'tenant_audit'
    )
    AND context->>'result' IN (
      'deleted', 'empty', 'locked', 'blocked_shared_reference',
      'blocked_inflight', 'not_due', 'not_deleted', 'failed'
    )
    AND jsonb_typeof(context->'blocked') = 'number'
    AND context->>'blocked' ~ '^[0-9]+$'
  );

RESET ROLE;
SET ROLE tgm_owner;
GRANT CREATE ON SCHEMA public TO tenant_purge_batcher;
RESET ROLE;
SET ROLE tenant_purge_batcher;

CREATE OR REPLACE FUNCTION public.tenant_purge_batch(tenant_key bigint, purge_category text)
RETURNS TABLE (result text, rows_deleted bigint, blocked bigint)
LANGUAGE plpgsql
SECURITY DEFINER
VOLATILE
PARALLEL UNSAFE
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
  cutoff timestamptz;
  category text := purge_category;
  deleted_count bigint := 0;
  blocked_count bigint := 0;
  identity_key bigint;
BEGIN
  IF session_user <> 'tenant_purge'
    OR current_user <> 'tenant_purge_batcher'
    OR current_setting('transaction_isolation') <> 'read committed'
    OR tenant_key <= 0
  THEN
    RAISE EXCEPTION 'invalid tenant purge batch context' USING ERRCODE = '42501';
  END IF;

  IF category NOT IN (
    'identity_dependents', 'rule_dependents', 'configuration',
    'notifications', 'terminal_delivery', 'tenant_events', 'tenant_audit'
  ) THEN
    RAISE EXCEPTION 'invalid tenant purge category' USING ERRCODE = '22023';
  END IF;

  IF NOT pg_catalog.pg_try_advisory_xact_lock(
    pg_catalog.hashtextextended('tenant_lifecycle:' || tenant_key::text, 419871)
  ) THEN
    result := 'locked'; rows_deleted := 0; blocked := 0;
    INSERT INTO public.system_logs (tenant_id, level, module, event, context)
    VALUES (NULL, 'INFO', 'tenant_purge', 'cleanup.done',
      pg_catalog.jsonb_build_object(
        'tenant_id', tenant_key, 'category', category, 'result', result,
        'rows', 0, 'blocked', 0
      ));
    RETURN NEXT;
    RETURN;
  END IF;

  PERFORM pg_catalog.set_config('app.purge_tenant_id', tenant_key::text, true);
  SELECT pg_catalog.statement_timestamp() INTO cutoff;

  IF NOT EXISTS (
    SELECT 1 FROM public.tenants
     WHERE id = tenant_key AND status = 'deleted'
  ) THEN
    result := 'not_deleted'; rows_deleted := 0; blocked := 0;
    INSERT INTO public.system_logs (tenant_id, level, module, event, context)
    VALUES (NULL, 'INFO', 'tenant_purge', 'cleanup.done',
      pg_catalog.jsonb_build_object(
        'tenant_id', tenant_key, 'category', category, 'result', result,
        'rows', 0, 'blocked', 0
      ));
    RETURN NEXT;
    RETURN;
  END IF;
  IF NOT EXISTS (
    SELECT 1 FROM public.tenants
     WHERE id = tenant_key AND deleted_at IS NOT NULL
       AND deleted_at <= cutoff - pg_catalog.interval '30 days'
  ) THEN
    result := 'not_due'; rows_deleted := 0; blocked := 0;
    INSERT INTO public.system_logs (tenant_id, level, module, event, context)
    VALUES (NULL, 'INFO', 'tenant_purge', 'cleanup.done',
      pg_catalog.jsonb_build_object(
        'tenant_id', tenant_key, 'category', category, 'result', result,
        'rows', 0, 'blocked', 0
      ));
    RETURN NEXT;
    RETURN;
  END IF;

  IF category = 'identity_dependents' THEN
    WITH candidates AS MATERIALIZED (
      SELECT secrets.identity_id
        FROM public.identity_secrets secrets
        JOIN public.identities identities ON identities.id = secrets.identity_id
       WHERE identities.tenant_id = tenant_key
       ORDER BY secrets.identity_id
       LIMIT 5000
    )
    DELETE FROM public.identity_secrets secrets
     USING candidates
     WHERE secrets.identity_id = candidates.identity_id;
    GET DIAGNOSTICS deleted_count = ROW_COUNT;

    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT leases.identity_id
          FROM public.account_leases leases
          JOIN public.identities identities ON identities.id = leases.identity_id
         WHERE identities.tenant_id = tenant_key
         ORDER BY leases.identity_id
         LIMIT 5000
      )
      DELETE FROM public.account_leases leases
       USING candidates
       WHERE leases.identity_id = candidates.identity_id;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT tenant_id FROM public.credential_keys
         WHERE tenant_id = tenant_key LIMIT 5000
      )
      DELETE FROM public.credential_keys target USING candidates
       WHERE target.tenant_id = candidates.tenant_id;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
  ELSIF category = 'rule_dependents' THEN
    WITH candidates AS MATERIALIZED (
      SELECT conditions.id
        FROM public.rule_conditions conditions
        JOIN public.rules rules ON rules.id = conditions.rule_id
       WHERE rules.tenant_id = tenant_key
       ORDER BY conditions.id
       LIMIT 5000
    )
    DELETE FROM public.rule_conditions conditions USING candidates
     WHERE conditions.id = candidates.id;
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT replacements.tenant_id, replacements.rule_id, replacements.seq
          FROM public.rule_replacements replacements
         WHERE replacements.tenant_id = tenant_key
         ORDER BY replacements.rule_id, replacements.seq
         LIMIT 5000
      )
      DELETE FROM public.rule_replacements replacements USING candidates
       WHERE replacements.tenant_id = candidates.tenant_id
         AND replacements.rule_id = candidates.rule_id
         AND replacements.seq = candidates.seq;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT versions.tenant_id, versions.rule_id, versions.version
          FROM public.rule_versions versions
         WHERE versions.tenant_id = tenant_key
         ORDER BY versions.rule_id, versions.version
         LIMIT 5000
      )
      DELETE FROM public.rule_versions versions USING candidates
       WHERE versions.tenant_id = candidates.tenant_id
         AND versions.rule_id = candidates.rule_id
         AND versions.version = candidates.version;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
  ELSIF category = 'configuration' THEN
    WITH candidates AS MATERIALIZED (
      SELECT rules.id
        FROM public.rules rules
       WHERE rules.tenant_id = tenant_key
       ORDER BY rules.id
       LIMIT 5000
    )
    DELETE FROM public.rules rules USING candidates
     WHERE rules.id = candidates.id;
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT id FROM public.keywords WHERE tenant_id = tenant_key ORDER BY id LIMIT 5000
      )
      DELETE FROM public.keywords target USING candidates
       WHERE target.id = candidates.id AND target.tenant_id = tenant_key;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT id FROM public.source_subscriptions WHERE tenant_id = tenant_key ORDER BY id LIMIT 5000
      )
      DELETE FROM public.source_subscriptions target USING candidates
       WHERE target.id = candidates.id AND target.tenant_id = tenant_key;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT id FROM public.target_channels WHERE tenant_id = tenant_key ORDER BY id LIMIT 5000
      )
      DELETE FROM public.target_channels target USING candidates
       WHERE target.id = candidates.id AND target.tenant_id = tenant_key;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
  ELSIF category = 'notifications' THEN
    WITH candidates AS MATERIALIZED (
      SELECT tenant_id, id FROM public.tenant_notifications
       WHERE tenant_id = tenant_key ORDER BY id LIMIT 5000
    )
    DELETE FROM public.tenant_notifications target USING candidates
     WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id;
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT tenant_id, fingerprint, target_ref FROM public.notify_dedup
         WHERE tenant_id = tenant_key ORDER BY fingerprint, target_ref LIMIT 5000
      )
      DELETE FROM public.notify_dedup target USING candidates
       WHERE target.tenant_id = candidates.tenant_id
         AND target.fingerprint = candidates.fingerprint
         AND target.target_ref = candidates.target_ref;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
  ELSIF category = 'terminal_delivery' THEN
    SELECT count(*) INTO blocked_count
      FROM public.delivery_tasks tasks
     WHERE tasks.tenant_id = tenant_key
       AND tasks.status IN ('PENDING', 'SENDING', 'FAILED');
    IF blocked_count > 0 THEN
      result := 'blocked_inflight'; rows_deleted := 0; blocked := blocked_count;
      INSERT INTO public.system_logs (tenant_id, level, module, event, context)
      VALUES (
        NULL, 'INFO', 'tenant_purge', 'cleanup.done',
        pg_catalog.jsonb_build_object(
          'tenant_id', tenant_key, 'category', category, 'result', result,
          'rows', 0, 'blocked', blocked_count
        )
      );
      RETURN NEXT;
      RETURN;
    END IF;
    WITH candidates AS MATERIALIZED (
      SELECT logs.id
        FROM public.delivery_logs logs
        JOIN public.delivery_tasks tasks ON tasks.id = logs.task_id
       WHERE tasks.tenant_id = tenant_key
       ORDER BY logs.id
       LIMIT 5000
    )
    DELETE FROM public.delivery_logs logs USING candidates
     WHERE logs.id = candidates.id;
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT tasks.id
          FROM public.delivery_tasks tasks
         WHERE tasks.tenant_id = tenant_key
           AND tasks.status IN ('DONE', 'DEAD', 'CANCELLED')
           AND NOT EXISTS (SELECT 1 FROM public.delivery_logs logs WHERE logs.task_id = tasks.id)
         ORDER BY tasks.id
         LIMIT 5000
      )
      DELETE FROM public.delivery_tasks tasks USING candidates
       WHERE tasks.id = candidates.id AND tasks.tenant_id = tenant_key;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
  ELSIF category = 'tenant_events' THEN
    IF NOT pg_catalog.pg_try_advisory_xact_lock(
      pg_catalog.hashtextextended('tenant_retention:' || tenant_key::text || ':event_lifecycle', 1952935218)
    ) THEN
      result := 'locked'; rows_deleted := 0; blocked := 0;
      INSERT INTO public.system_logs (tenant_id, level, module, event, context)
      VALUES (NULL, 'INFO', 'tenant_purge', 'cleanup.done',
        pg_catalog.jsonb_build_object(
          'tenant_id', tenant_key, 'category', category, 'result', result,
          'rows', 0, 'blocked', 0
        ));
      RETURN NEXT;
      RETURN;
    END IF;
    FOR identity_key IN
      SELECT id FROM public.identities WHERE tenant_id = tenant_key ORDER BY id
    LOOP
      IF NOT pg_catalog.pg_try_advisory_xact_lock(
        pg_catalog.hashtextextended('shared_retention:identity:' || identity_key::text, 1952935218)
      ) THEN
        result := 'locked'; rows_deleted := 0; blocked := 0;
        INSERT INTO public.system_logs (tenant_id, level, module, event, context)
        VALUES (NULL, 'INFO', 'tenant_purge', 'cleanup.done',
          pg_catalog.jsonb_build_object(
            'tenant_id', tenant_key, 'category', category, 'result', result,
            'rows', 0, 'blocked', 0
          ));
        RETURN NEXT;
        RETURN;
      END IF;
    END LOOP;

    IF EXISTS (SELECT 1 FROM public.tenant_events WHERE tenant_id = tenant_key) THEN
      WITH candidates AS MATERIALIZED (
        SELECT events.id, events.canonical_event_id
          FROM public.tenant_events events
         WHERE events.tenant_id = tenant_key
         ORDER BY events.id
         LIMIT 5000
      )
      SELECT pg_catalog.count(*) INTO blocked_count
        FROM candidates
       WHERE EXISTS (
         SELECT 1 FROM public.tenant_events refs
          WHERE refs.canonical_event_id = candidates.canonical_event_id
            AND refs.tenant_id <> tenant_key
       )
          OR EXISTS (
         SELECT 1 FROM public.media_group_members members
          WHERE members.group_event_id = candidates.canonical_event_id
             OR members.member_event_id = candidates.canonical_event_id
       )
          OR EXISTS (
         SELECT 1 FROM public.media_relay_jobs jobs
          WHERE jobs.canonical_event_id = candidates.canonical_event_id
            AND jobs.status IN ('PENDING', 'DOWNLOADING', 'READY')
       );

      WITH candidates AS MATERIALIZED (
        SELECT events.id
          FROM public.tenant_events events
         WHERE events.tenant_id = tenant_key
           AND NOT EXISTS (
             SELECT 1 FROM public.tenant_events refs
              WHERE refs.canonical_event_id = events.canonical_event_id
                AND refs.tenant_id <> tenant_key
           )
           AND NOT EXISTS (
             SELECT 1 FROM public.media_group_members members
              WHERE members.group_event_id = events.canonical_event_id
                 OR members.member_event_id = events.canonical_event_id
           )
           AND NOT EXISTS (
             SELECT 1 FROM public.media_relay_jobs jobs
              WHERE jobs.canonical_event_id = events.canonical_event_id
                AND jobs.status IN ('PENDING', 'DOWNLOADING', 'READY')
           )
         ORDER BY events.id
         LIMIT 5000
      )
      DELETE FROM public.tenant_events events USING candidates
       WHERE events.id = candidates.id AND events.tenant_id = tenant_key;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    ELSE
      SELECT pg_catalog.count(*) INTO blocked_count
        FROM public.canonical_events events
       WHERE events.receiver_identity_id IN (
         SELECT id FROM public.identities WHERE tenant_id = tenant_key
       )
         AND (
           EXISTS (
             SELECT 1 FROM public.tenant_events refs
              WHERE refs.canonical_event_id = events.id
           )
           OR EXISTS (
             SELECT 1 FROM public.media_group_members members
              WHERE members.group_event_id = events.id OR members.member_event_id = events.id
           )
           OR EXISTS (
             SELECT 1 FROM public.media_relay_jobs jobs
              WHERE jobs.canonical_event_id = events.id
                AND jobs.status IN ('PENDING', 'DOWNLOADING', 'READY')
           )
         );
      WITH candidates AS MATERIALIZED (
        SELECT events.id
          FROM public.canonical_events events
         WHERE events.receiver_identity_id IN (
           SELECT id FROM public.identities WHERE tenant_id = tenant_key
         )
           AND NOT EXISTS (
             SELECT 1 FROM public.tenant_events refs
              WHERE refs.canonical_event_id = events.id
           )
           AND NOT EXISTS (
             SELECT 1 FROM public.media_group_members members
              WHERE members.group_event_id = events.id OR members.member_event_id = events.id
           )
           AND NOT EXISTS (
             SELECT 1 FROM public.media_relay_jobs jobs
              WHERE jobs.canonical_event_id = events.id
                AND jobs.status IN ('PENDING', 'DOWNLOADING', 'READY')
           )
         ORDER BY events.id
         LIMIT 5000
      )
      DELETE FROM public.canonical_events events USING candidates
       WHERE events.id = candidates.id;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
  ELSIF category = 'tenant_audit' THEN
    WITH candidates AS MATERIALIZED (
      SELECT id FROM public.login_sessions WHERE tenant_id = tenant_key ORDER BY id LIMIT 5000
    )
    DELETE FROM public.login_sessions target USING candidates
     WHERE target.id = candidates.id AND target.tenant_id = tenant_key;
    GET DIAGNOSTICS deleted_count = ROW_COUNT;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT tg_user_id FROM public.bot_console_sessions WHERE tenant_id = tenant_key ORDER BY tg_user_id LIMIT 5000
      )
      DELETE FROM public.bot_console_sessions target USING candidates
       WHERE target.tg_user_id = candidates.tg_user_id AND target.tenant_id = tenant_key;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT tenant_id, bot_id, update_id FROM public.bot_console_updates
         WHERE tenant_id = tenant_key ORDER BY bot_id, update_id LIMIT 5000
      )
      DELETE FROM public.bot_console_updates target USING candidates
       WHERE target.tenant_id = candidates.tenant_id AND target.bot_id = candidates.bot_id
         AND target.update_id = candidates.update_id;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT tenant_id, id FROM public.web_sessions WHERE tenant_id = tenant_key ORDER BY id LIMIT 5000
      )
      DELETE FROM public.web_sessions target USING candidates
       WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT tenant_id, id FROM public.audit_logs WHERE tenant_id = tenant_key ORDER BY id LIMIT 5000
      )
      DELETE FROM public.audit_logs target USING candidates
       WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT tenant_id, id FROM public.system_logs WHERE tenant_id = tenant_key ORDER BY id LIMIT 5000
      )
      DELETE FROM public.system_logs target USING candidates
       WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0
      AND NOT EXISTS (SELECT 1 FROM public.tenant_events WHERE tenant_id = tenant_key)
      AND NOT EXISTS (
        SELECT 1 FROM public.source_subscriptions
         WHERE tenant_id <> tenant_key
           AND receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.target_channels
         WHERE tenant_id <> tenant_key
           AND sender_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.delivery_tasks
         WHERE tenant_id <> tenant_key
           AND sender_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.canonical_events
         WHERE receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.media_group_members members
         WHERE members.group_event_id IN (
           SELECT id FROM public.canonical_events
            WHERE receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
         )
            OR members.member_event_id IN (
           SELECT id FROM public.canonical_events
            WHERE receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
         )
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.media_relay_jobs jobs
           WHERE jobs.status IN ('PENDING', 'DOWNLOADING', 'READY')
           AND (
             jobs.receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
              OR jobs.canonical_event_id IN (
              SELECT id FROM public.canonical_events
               WHERE receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
              )
           )
      )
    THEN
      WITH candidates AS MATERIALIZED (
        SELECT tenant_id, id FROM public.tenant_consents
         WHERE tenant_id = tenant_key ORDER BY id LIMIT 5000
      )
      DELETE FROM public.tenant_consents target USING candidates
       WHERE target.tenant_id = candidates.tenant_id AND target.id = candidates.id;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0
      AND NOT EXISTS (SELECT 1 FROM public.tenant_events WHERE tenant_id = tenant_key)
      AND NOT EXISTS (
        SELECT 1 FROM public.source_subscriptions
         WHERE tenant_id <> tenant_key
           AND receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.target_channels
         WHERE tenant_id <> tenant_key
           AND sender_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.delivery_tasks
         WHERE tenant_id <> tenant_key
           AND sender_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.canonical_events
         WHERE receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.media_group_members members
         WHERE members.group_event_id IN (
           SELECT id FROM public.canonical_events
            WHERE receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
         )
            OR members.member_event_id IN (
           SELECT id FROM public.canonical_events
            WHERE receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
         )
      )
      AND NOT EXISTS (
        SELECT 1 FROM public.media_relay_jobs jobs
           WHERE jobs.status IN ('PENDING', 'DOWNLOADING', 'READY')
           AND (
             jobs.receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
              OR jobs.canonical_event_id IN (
              SELECT id FROM public.canonical_events
               WHERE receiver_identity_id IN (SELECT id FROM public.identities WHERE tenant_id = tenant_key)
              )
           )
      )
    THEN
      WITH candidates AS MATERIALIZED (
        SELECT tenant_id, user_id FROM public.tenant_members
         WHERE tenant_id = tenant_key ORDER BY user_id LIMIT 5000
      )
      DELETE FROM public.tenant_members target USING candidates
       WHERE target.tenant_id = candidates.tenant_id AND target.user_id = candidates.user_id;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
  END IF;

  IF deleted_count = 0 AND blocked_count = 0 THEN
    result := 'empty';
  ELSE
    result := CASE
      WHEN blocked_count > 0 THEN 'blocked_shared_reference'
      ELSE 'deleted'
    END;
  END IF;
  rows_deleted := deleted_count;
  blocked := blocked_count;

  INSERT INTO public.system_logs (tenant_id, level, module, event, context)
  VALUES (
    NULL, 'INFO', 'tenant_purge', 'cleanup.done',
    pg_catalog.jsonb_build_object(
      'tenant_id', tenant_key, 'category', category, 'result', result,
      'rows', deleted_count, 'blocked', blocked_count
    )
  );
  RETURN NEXT;
EXCEPTION
  WHEN OTHERS THEN
    RAISE;
END
$$;

ALTER FUNCTION public.tenant_purge_batch(bigint, text) OWNER TO tenant_purge_batcher;
REVOKE ALL ON FUNCTION public.tenant_purge_batch(bigint, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.tenant_purge_batch(bigint, text) TO tenant_purge;
RESET ROLE;
SET ROLE tgm_owner;
REVOKE tenant_purge_batcher FROM tgm_owner;
REVOKE CREATE ON SCHEMA public FROM tenant_purge_batcher;
RESET ROLE;
