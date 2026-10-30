SET ROLE tgm_owner;

CREATE OR REPLACE FUNCTION public.tenant_lifecycle_secret_delete() RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  IF current_user = 'tenant_lifecycle'
    AND (
      current_setting('app.tenant_id', true) IS NULL
      OR NOT EXISTS (
        SELECT 1
          FROM public.identities identity_row
          JOIN public.tenants tenant_row ON tenant_row.id = identity_row.tenant_id
         WHERE identity_row.id = OLD.identity_id
           AND identity_row.tenant_id = current_setting('app.tenant_id', true)::bigint
           AND tenant_row.status = 'deleted'
      )
    )
  THEN
    RAISE EXCEPTION 'invalid lifecycle secret delete' USING ERRCODE = '42501';
  END IF;
  RETURN OLD;
END
$$;

CREATE OR REPLACE FUNCTION public.invalidate_identity_target_probes() RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
  identity_key bigint;
  tenant_key bigint;
BEGIN
  IF current_user = 'tenant_purge_batcher' THEN
    RETURN OLD;
  END IF;
  IF TG_TABLE_NAME = 'identity_secrets' THEN
    identity_key := NEW.identity_id;
    IF TG_OP = 'UPDATE' AND NEW.secret_ciphertext IS NOT DISTINCT FROM OLD.secret_ciphertext
       AND NEW.nonce IS NOT DISTINCT FROM OLD.nonce
       AND NEW.dek_ciphertext IS NOT DISTINCT FROM OLD.dek_ciphertext
       AND NEW.kek_id IS NOT DISTINCT FROM OLD.kek_id THEN
      RETURN NEW;
    END IF;
    SELECT tenant_id INTO tenant_key FROM public.identities WHERE id = identity_key;
  ELSE
    identity_key := NEW.id;
    tenant_key := NEW.tenant_id;
    IF NEW.kind IS NOT DISTINCT FROM OLD.kind
       AND NEW.tg_user_id IS NOT DISTINCT FROM OLD.tg_user_id
       AND (NEW.status IN ('banned', 'unauthorized')) = (OLD.status IN ('banned', 'unauthorized')) THEN
      RETURN NEW;
    END IF;
  END IF;
  IF EXISTS (SELECT 1 FROM public.tenants WHERE id = tenant_key AND status = 'deleted') THEN
    RETURN NEW;
  END IF;
  UPDATE public.target_channels
     SET probe_generation = probe_generation + 1,
         probe_state = 'PENDING',
         probe_attempts = 0,
         probe_due_at = clock_timestamp(),
         probe_token = NULL,
         probe_expires_at = NULL,
         probe_error_code = NULL,
         can_send = NULL,
         checked_at = NULL,
         content_protected = NULL,
         bot_role = NULL
   WHERE tenant_id = tenant_key AND sender_identity_id = identity_key;
  RETURN NEW;
END
$$;

GRANT SELECT (id, tenant_id, receiver_identity_id) ON public.source_subscriptions TO tenant_purge_batcher;
GRANT SELECT (id, tenant_id, sender_identity_id) ON public.target_channels TO tenant_purge_batcher;
GRANT SELECT (tenant_id, id, sender_identity_id, tenant_event_id, status, updated_at)
  ON public.delivery_tasks TO tenant_purge_batcher;
GRANT SELECT (tenant_id, bot_id, update_id) ON public.bot_console_updates TO tenant_purge_batcher;
GRANT SELECT (tenant_id, identity_id) ON public.tenant_notifications TO tenant_purge_batcher;
GRANT DELETE ON public.canonical_events TO tenant_purge_batcher;

DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.source_subscriptions;
CREATE POLICY tenant_purge_batcher_read ON public.source_subscriptions
  FOR SELECT TO tenant_purge_batcher
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR receiver_identity_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.target_channels;
CREATE POLICY tenant_purge_batcher_read ON public.target_channels
  FOR SELECT TO tenant_purge_batcher
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR sender_identity_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.delivery_tasks;
CREATE POLICY tenant_purge_batcher_read ON public.delivery_tasks
  FOR SELECT TO tenant_purge_batcher
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR sender_identity_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.bot_console_updates;
CREATE POLICY tenant_purge_batcher_read ON public.bot_console_updates
  FOR SELECT TO tenant_purge_batcher
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR bot_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );
DROP POLICY IF EXISTS tenant_purge_batcher_read ON public.tenant_notifications;
CREATE POLICY tenant_purge_batcher_read ON public.tenant_notifications
  FOR SELECT TO tenant_purge_batcher
  USING (
    tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    OR identity_id IN (
      SELECT id FROM public.identities
       WHERE tenant_id = NULLIF(current_setting('app.purge_tenant_id', true), '')::bigint
    )
  );

RESET ROLE;
GRANT tenant_purge_batcher TO tgm_owner WITH ADMIN OPTION;
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
  inflight_count bigint := 0;
  failure_code text;
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
         AND NOT EXISTS (
           SELECT 1 FROM public.rule_conditions conditions
            WHERE conditions.rule_id = rules.id
         )
         AND NOT EXISTS (
           SELECT 1 FROM public.rule_replacements replacements
            WHERE replacements.tenant_id = tenant_key
              AND replacements.rule_id = rules.id
         )
         AND NOT EXISTS (
           SELECT 1 FROM public.rule_versions versions
            WHERE versions.tenant_id = tenant_key
              AND versions.rule_id = rules.id
         )
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
        SELECT id FROM public.source_subscriptions
         WHERE tenant_id = tenant_key
           AND NOT EXISTS (
             SELECT 1 FROM public.tenant_events events
              WHERE events.tenant_id = tenant_key
                AND events.source_ref = source_subscriptions.id
           )
         ORDER BY id LIMIT 5000
      )
      DELETE FROM public.source_subscriptions target USING candidates
       WHERE target.id = candidates.id AND target.tenant_id = tenant_key;
      GET DIAGNOSTICS deleted_count = ROW_COUNT;
    END IF;
    IF deleted_count = 0 THEN
      WITH candidates AS MATERIALIZED (
        SELECT id FROM public.target_channels
         WHERE tenant_id = tenant_key
           AND NOT EXISTS (
             SELECT 1 FROM public.rules rules
              WHERE rules.tenant_id = tenant_key
                AND rules.target_ref = target_channels.id
           )
         ORDER BY id LIMIT 5000
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
      SELECT count(*) FILTER (WHERE EXISTS (
               SELECT 1 FROM public.delivery_tasks tasks
                WHERE tasks.tenant_id = tenant_key
                  AND tasks.tenant_event_id = candidates.id
             )),
             count(*) FILTER (WHERE EXISTS (
               SELECT 1 FROM public.delivery_tasks tasks
                WHERE tasks.tenant_id = tenant_key
                  AND tasks.tenant_event_id = candidates.id
             ) OR EXISTS (
               SELECT 1 FROM public.tenant_events refs
                WHERE refs.canonical_event_id = candidates.canonical_event_id
                  AND refs.tenant_id <> tenant_key
             ) OR EXISTS (
               SELECT 1 FROM public.media_group_members members
                WHERE members.group_event_id = candidates.canonical_event_id
                   OR members.member_event_id = candidates.canonical_event_id
             ) OR EXISTS (
               SELECT 1 FROM public.media_relay_jobs jobs
                WHERE jobs.canonical_event_id = candidates.canonical_event_id
                  AND jobs.status IN ('PENDING', 'DOWNLOADING', 'READY')
             ))
        INTO inflight_count, blocked_count
        FROM candidates;

      WITH candidates AS MATERIALIZED (
        SELECT events.id
          FROM public.tenant_events events
         WHERE events.tenant_id = tenant_key
           AND NOT EXISTS (
             SELECT 1 FROM public.delivery_tasks tasks
              WHERE tasks.tenant_id = tenant_key
                AND tasks.tenant_event_id = events.id
           )
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
         SELECT tenant_id, user_id FROM public.tenant_members members
          WHERE tenant_id = tenant_key
            AND NOT EXISTS (
              SELECT 1 FROM public.bot_console_sessions sessions
               WHERE sessions.tenant_id = tenant_key
            )
            AND NOT EXISTS (
              SELECT 1 FROM public.web_sessions sessions
               WHERE sessions.tenant_id = tenant_key
            )
            AND NOT EXISTS (
              SELECT 1 FROM public.tenant_consents consents
               WHERE consents.tenant_id = tenant_key
            )
          ORDER BY user_id LIMIT 5000
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
      WHEN inflight_count > 0 THEN 'blocked_inflight'
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
    GET STACKED DIAGNOSTICS failure_code = RETURNED_SQLSTATE;
    IF failure_code IN ('22023', '42501') THEN
      RAISE;
    END IF;
    result := 'failed'; rows_deleted := 0; blocked := 0;
    INSERT INTO public.system_logs (tenant_id, level, module, event, context)
    VALUES (
      NULL, 'INFO', 'tenant_purge', 'cleanup.done',
      pg_catalog.jsonb_build_object(
        'tenant_id', tenant_key, 'category', category, 'result', result,
        'rows', 0, 'blocked', 0
      )
    );
    RETURN NEXT;
END
$$;

RESET ROLE;
REVOKE tenant_purge_batcher FROM tgm_owner CASCADE;
REVOKE CREATE ON SCHEMA public FROM tenant_purge_batcher;
RESET ROLE;
