use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::{Duration, Instant};
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::repo;
use tgm_db::shared_retention::{check_connection, cleanup_batch, lock_writer};

mod support;

async fn setup(
    injected: PgPool,
) -> Result<(PgPool, PgPool, support::AdminOnlyPool, i64), Box<dyn std::error::Error>> {
    let (app, admin) = support::pools(injected).await;
    let cleanup = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(
            admin
                .raw()
                .connect_options()
                .as_ref()
                .clone()
                .username("shared_cleanup")
                .password("scpw"),
        )
        .await?;
    support::assert_role(&cleanup, "shared_cleanup").await;
    support::assert_not_bypassrls(&cleanup).await;
    check_connection(&cleanup).await?;
    let identity = sqlx::query_scalar(
        "INSERT INTO identities (kind, status) VALUES ('mtproto','online') RETURNING id",
    )
    .fetch_one(admin.raw())
    .await?;
    sqlx::raw_sql(
        "INSERT INTO tenants (name,owner_user_id,plan) VALUES
        ('shared-free',97001,'free'),('shared-pro',97002,'pro');",
    )
    .execute(admin.raw())
    .await?;
    sqlx::query(
        "INSERT INTO source_subscriptions (tenant_id,tg_chat_id,receiver_identity_id)
                 SELECT id,-1001,$1 FROM tenants",
    )
    .bind(identity)
    .execute(admin.raw())
    .await?;
    Ok((cleanup, app, admin, identity))
}

async fn seed_event(
    admin: &PgPool,
    identity: i64,
    message: i64,
    age: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO canonical_events (adapter,receiver_identity_id,tg_chat_id,tg_message_id,
            message_date,created_at,fanned_out,text)
         VALUES ('mtproto',$1,-1001,$2,now(),now()-$3::interval,true,'private body') RETURNING id",
    )
    .bind(identity)
    .bind(message)
    .bind(age)
    .fetch_one(admin)
    .await
}

async fn count(admin: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM canonical_events")
        .fetch_one(admin)
        .await
}

async fn wait_for_lock(admin: &PgPool, role: &str) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS (
            SELECT 1 FROM pg_locks locks JOIN pg_stat_activity activity ON activity.pid=locks.pid
             WHERE activity.datname=current_database() AND activity.usename=$1 AND NOT locks.granted)")
            .bind(role).fetch_one(admin).await?;
        if waiting {
            return Ok(());
        }
        assert!(Instant::now() < deadline, "未观察到 {role} 的真实锁等待");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn respects_age_fanout_and_all_tenant_references(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, app, admin, identity) = setup(injected).await?;
    let old = seed_event(admin.raw(), identity, 1, "31 days").await?;
    let recent = seed_event(admin.raw(), identity, 2, "23 hours").await?;
    let matched = seed_event(admin.raw(), identity, 3, "29 days").await?;
    let matched_old = seed_event(admin.raw(), identity, 4, "31 days").await?;
    let unfanned = seed_event(admin.raw(), identity, 5, "31 days").await?;
    sqlx::query("UPDATE canonical_events SET matched_any=true WHERE id=ANY($1)")
        .bind(vec![matched, matched_old])
        .execute(admin.raw())
        .await?;
    sqlx::query("UPDATE canonical_events SET fanned_out=false WHERE id=$1")
        .bind(unfanned)
        .execute(admin.raw())
        .await?;
    let tenants: Vec<TenantId> = sqlx::query_scalar("SELECT id FROM tenants ORDER BY id")
        .fetch_all(admin.raw())
        .await?;
    sqlx::query("UPDATE canonical_events SET fanned_out=false WHERE id=$1")
        .bind(old)
        .execute(admin.raw())
        .await?;
    for tenant in &tenants {
        repo::fanout_canonical_event(&app, *tenant, old).await?;
    }
    repo::mark_canonical_event_fanned_out(&app, old).await?;
    sqlx::query("UPDATE tenant_events SET processed_at=now(),matched=false,created_at=now()-interval '8 days'")
        .execute(admin.raw()).await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(1));
    assert_eq!(count(admin.raw()).await?, 4);
    for context in ["0", "1", "999999"] {
        let mut transaction = cleanup.begin().await?;
        sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
            .bind(context)
            .execute(&mut *transaction)
            .await?;
        let references: Vec<i64> =
            sqlx::query_scalar("SELECT canonical_event_id FROM tenant_events")
                .fetch_all(&mut *transaction)
                .await?;
        assert_eq!(references, vec![old, old]);
        transaction.rollback().await?;
    }
    let tenant_cleanup = PgPoolOptions::new()
        .connect_with(
            admin
                .raw()
                .connect_options()
                .as_ref()
                .clone()
                .username("tenant_cleanup")
                .password("tcpw"),
        )
        .await?;
    assert_eq!(
        tgm_db::tenant_retention::cleanup_batch(
            &tenant_cleanup,
            tenants[0],
            tgm_db::tenant_retention::Category::TenantEvents
        )
        .await?,
        Some(1)
    );
    assert_eq!(cleanup_batch(&cleanup).await?, Some(0));
    assert_eq!(count(admin.raw()).await?, 4);
    let sender: i64 = sqlx::query_scalar(
        "INSERT INTO identities (tenant_id,kind) VALUES ($1,'bot') RETURNING id",
    )
    .bind(tenants[1].get())
    .fetch_one(admin.raw())
    .await?;
    let task: i64 = sqlx::query_scalar(
        "INSERT INTO delivery_tasks (tenant_id,tenant_event_id,sender_identity_id,target_tg_chat_id,action,status)
         SELECT tenant_id,id,$1,-3001,'COPY','SENDING' FROM tenant_events WHERE tenant_id=$2 RETURNING id",
    )
    .bind(sender)
    .bind(tenants[1].get())
    .fetch_one(admin.raw())
    .await?;
    sqlx::query("UPDATE tenant_events SET created_at=now()-interval '31 days'")
        .execute(admin.raw())
        .await?;
    assert_eq!(
        tgm_db::tenant_retention::cleanup_batch(
            &tenant_cleanup,
            tenants[1],
            tgm_db::tenant_retention::Category::TenantEvents
        )
        .await?,
        Some(0)
    );
    assert_eq!(cleanup_batch(&cleanup).await?, Some(0));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM delivery_tasks WHERE id=$1")
            .bind(task)
            .fetch_one(admin.raw())
            .await?,
        "SENDING"
    );
    sqlx::query("UPDATE delivery_tasks SET status='DONE',completed_at=now() WHERE id=$1")
        .bind(task)
        .execute(admin.raw())
        .await?;
    assert_eq!(
        tgm_db::tenant_retention::cleanup_batch(
            &tenant_cleanup,
            tenants[1],
            tgm_db::tenant_retention::Category::TenantEvents
        )
        .await?,
        Some(1)
    );
    assert_eq!(cleanup_batch(&cleanup).await?, Some(1));
    assert_eq!(count(admin.raw()).await?, 3);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM canonical_events WHERE id=$1)")
            .bind(recent)
            .fetch_one(admin.raw())
            .await?
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn group_references_and_live_downloads_survive_then_release(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, app, admin, identity) = setup(injected).await?;
    let group = seed_event(admin.raw(), identity, 1, "32 days").await?;
    let member = seed_event(admin.raw(), identity, 2, "32 days").await?;
    sqlx::query("UPDATE canonical_events SET event_type='MEDIA_GROUP_COMPLETED',media_group_id='album' WHERE id=$1")
        .bind(group).execute(admin.raw()).await?;
    sqlx::query("INSERT INTO media_group_members (group_event_id,member_event_id,position) VALUES ($1,$2,0)")
        .bind(group).bind(member).execute(&app).await?;
    sqlx::query("INSERT INTO media_relay_jobs (canonical_event_id,receiver_identity_id,status,next_retry_at)
                VALUES ($1,$2,'DOWNLOADING',now()+interval '1 hour')")
        .bind(member).bind(identity).execute(&app).await?;
    let tenant: TenantId = sqlx::query_scalar("SELECT id FROM tenants LIMIT 1")
        .fetch_one(admin.raw())
        .await?;
    sqlx::query("UPDATE canonical_events SET fanned_out=false WHERE id=$1")
        .bind(group)
        .execute(admin.raw())
        .await?;
    repo::fanout_canonical_event(&app, tenant, group).await?;
    repo::mark_canonical_event_fanned_out(&app, group).await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(0));
    assert!(
        sqlx::query("DELETE FROM canonical_events WHERE id=$1")
            .bind(group)
            .execute(&app)
            .await
            .is_err()
    );
    for (mutation, target) in [
        (
            "ALTER TABLE tenant_events DROP CONSTRAINT tenant_events_canonical_event_id_fkey;
             ALTER TABLE tenant_events ADD CONSTRAINT tenant_events_canonical_event_id_fkey
             FOREIGN KEY (canonical_event_id) REFERENCES canonical_events(id) ON DELETE CASCADE;",
            group,
        ),
        (
            "ALTER TABLE media_group_members DROP CONSTRAINT media_group_members_member_event_id_fkey;
             ALTER TABLE media_group_members ADD CONSTRAINT media_group_members_member_event_id_fkey
             FOREIGN KEY (member_event_id) REFERENCES canonical_events(id) ON DELETE CASCADE;",
            member,
        ),
    ] {
        let mut transaction = admin.raw().begin().await?;
        sqlx::query("SET LOCAL ROLE tgm_owner")
            .execute(&mut *transaction)
            .await?;
        sqlx::raw_sql(mutation).execute(&mut *transaction).await?;
        sqlx::query("RESET ROLE")
            .execute(&mut *transaction)
            .await?;
        assert_eq!(
            sqlx::query("DELETE FROM canonical_events WHERE id=$1")
                .bind(target)
                .execute(&mut *transaction)
                .await?
                .rows_affected(),
            1
        );
        transaction.rollback().await?;
    }
    assert!(
        sqlx::query("DELETE FROM canonical_events WHERE id=$1")
            .bind(member)
            .execute(&app)
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM tenant_events")
        .execute(admin.raw())
        .await?;
    sqlx::query("UPDATE canonical_events SET fanned_out=false WHERE id=$1")
        .bind(group)
        .execute(admin.raw())
        .await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(0));
    sqlx::query("UPDATE canonical_events SET fanned_out=true WHERE id=$1")
        .bind(group)
        .execute(admin.raw())
        .await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(1));
    assert_eq!(cleanup_batch(&cleanup).await?, Some(0));
    sqlx::query("UPDATE media_relay_jobs SET next_retry_at=NULL")
        .execute(&app)
        .await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(0));
    sqlx::query("UPDATE media_relay_jobs SET next_retry_at=now()-interval '1 hour'")
        .execute(&app)
        .await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(1));
    assert_eq!(count(admin.raw()).await?, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_relay_jobs")
            .fetch_one(&app)
            .await?,
        0
    );
    let ready = seed_event(admin.raw(), identity, 3, "32 days").await?;
    let archive: i64 = sqlx::query_scalar(
        "INSERT INTO media_archive (content_hash,media_type,object_key)
        VALUES ('shared-ready-hash','PHOTO','archive/keep') RETURNING id",
    )
    .fetch_one(&app)
    .await?;
    sqlx::query("INSERT INTO media_relay_jobs (canonical_event_id,receiver_identity_id,status,media_archive_id)
        VALUES ($1,$2,'READY',$3)")
        .bind(ready).bind(identity).bind(archive).execute(&app).await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(1));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT object_key FROM media_archive WHERE id=$1")
            .bind(archive)
            .fetch_one(&app)
            .await?,
        "archive/keep"
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn reference_policy_limits_recent_metadata_and_broadening_is_detected(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, app, admin, identity) = setup(injected).await?;
    let recent = seed_event(admin.raw(), identity, 1, "1 hour").await?;
    let tenant: TenantId = sqlx::query_scalar("SELECT id FROM tenants LIMIT 1")
        .fetch_one(admin.raw())
        .await?;
    sqlx::query("UPDATE canonical_events SET fanned_out=false WHERE id=$1")
        .bind(recent)
        .execute(admin.raw())
        .await?;
    repo::fanout_canonical_event(&app, tenant, recent).await?;
    repo::mark_canonical_event_fanned_out(&app, recent).await?;
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT canonical_event_id FROM tenant_events")
            .fetch_all(&cleanup)
            .await?
            .is_empty()
    );
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER POLICY shared_cleanup_select ON tenant_events USING (true); RESET ROLE;")
        .execute(admin.raw()).await?;
    let broadened = sqlx::query_scalar::<_, i64>("SELECT canonical_event_id FROM tenant_events")
        .fetch_all(&cleanup)
        .await;
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER POLICY shared_cleanup_select ON tenant_events USING (
        EXISTS (SELECT 1 FROM canonical_events parent WHERE parent.id=tenant_events.canonical_event_id
          AND parent.created_at <= statement_timestamp()-interval '24 hours')); RESET ROLE;")
        .execute(admin.raw()).await?;
    assert_eq!(broadened?, vec![recent]);
    assert!(
        sqlx::query_scalar::<_, i64>("SELECT canonical_event_id FROM tenant_events")
            .fetch_all(&cleanup)
            .await?
            .is_empty()
    );
    assert_eq!(cleanup_batch(&cleanup).await?, Some(0));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn least_privilege_logs_rollback_and_audit_counterexamples(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, _app, admin, identity) = setup(injected).await?;
    seed_event(admin.raw(), identity, 1, "2 days").await?;
    for query in [
        "SELECT text FROM canonical_events",
        "SELECT tenant_id FROM tenant_events",
        "SELECT object_key FROM media_archive",
        "SELECT * FROM identity_secrets",
        "DELETE FROM tenant_events",
        "TRUNCATE canonical_events",
        "UPDATE canonical_events SET fanned_out=true",
        "SELECT * FROM system_logs",
    ] {
        assert!(
            sqlx::query(query).execute(&cleanup).await.is_err(),
            "权限未限制 {query}"
        );
    }
    for context in [
        serde_json::json!({"table":"canonical_events","rows":5001,"elapsed_ms":0}),
        serde_json::json!({"table":"canonical_events","rows":1,"elapsed_ms":0,"body":"private"}),
    ] {
        assert!(sqlx::query("INSERT INTO system_logs (level,module,event,context) VALUES ('INFO','shared_retention','cleanup.done',$1)")
            .bind(context).execute(&cleanup).await.is_err());
    }
    sqlx::raw_sql("SET ROLE tgm_owner; ALTER TABLE system_logs ADD CONSTRAINT reject_shared CHECK (module <> 'shared_retention') NOT VALID; RESET ROLE;")
        .execute(admin.raw()).await?;
    assert!(cleanup_batch(&cleanup).await.is_err());
    assert_eq!(count(admin.raw()).await?, 1);
    sqlx::raw_sql(
        "SET ROLE tgm_owner; ALTER TABLE system_logs DROP CONSTRAINT reject_shared; RESET ROLE;",
    )
    .execute(admin.raw())
    .await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(1));
    let context: serde_json::Value =
        sqlx::query_scalar("SELECT context FROM system_logs WHERE module='shared_retention'")
            .fetch_one(admin.raw())
            .await?;
    assert_eq!(context.as_object().map(|object| object.len()), Some(3));
    let assertions = tgm_db::audit::parse(include_str!("../src/audit/queries.sql"));
    for (id, mutation) in [
        (
            "C10",
            "GRANT SELECT (text) ON canonical_events TO shared_cleanup",
        ),
        (
            "C10",
            "REVOKE SELECT (canonical_event_id) ON tenant_events FROM shared_cleanup",
        ),
        (
            "C10",
            "GRANT MAINTAIN ON canonical_events TO shared_cleanup",
        ),
        (
            "C10",
            "GRANT SELECT (id) ON canonical_events TO shared_cleanup WITH GRANT OPTION",
        ),
        (
            "C10",
            "GRANT SELECT ON SEQUENCE system_logs_id_seq TO shared_cleanup",
        ),
        (
            "C11",
            "ALTER TABLE tenant_events DROP CONSTRAINT tenant_events_canonical_event_id_fkey",
        ),
        (
            "C11",
            "ALTER TABLE tenant_events ADD CONSTRAINT duplicate_cascade FOREIGN KEY (canonical_event_id) REFERENCES canonical_events(id) ON DELETE CASCADE",
        ),
        (
            "C11",
            "ALTER TABLE media_group_members DROP CONSTRAINT media_group_members_member_event_id_fkey",
        ),
        (
            "C11",
            "ALTER TABLE media_group_members DROP CONSTRAINT media_group_members_group_event_id_fkey",
        ),
        (
            "C11",
            "ALTER TABLE media_relay_jobs DROP CONSTRAINT media_relay_jobs_canonical_event_id_fkey",
        ),
    ] {
        let assertion = assertions
            .iter()
            .find(|assertion| assertion.id == id)
            .ok_or("断言缺失")?;
        let mut transaction = admin.raw().begin().await?;
        sqlx::query("SET LOCAL ROLE tgm_owner")
            .execute(&mut *transaction)
            .await?;
        assert!(
            sqlx::query(sqlx::AssertSqlSafe(assertion.sql.as_str()))
                .fetch_all(&mut *transaction)
                .await?
                .is_empty()
        );
        sqlx::query(mutation).execute(&mut *transaction).await?;
        assert!(
            !sqlx::query(sqlx::AssertSqlSafe(assertion.sql.as_str()))
                .fetch_all(&mut *transaction)
                .await?
                .is_empty(),
            "审计漏报 {mutation}"
        );
        transaction.rollback().await?;
    }
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn writer_first_rechecks_reference_after_real_lock_wait(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, app, admin, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "2 days").await?;
    let mut writer = app.begin().await?;
    lock_writer(&mut writer, identity).await?;
    let pool = cleanup.clone();
    let task = tokio::spawn(async move { cleanup_batch(&pool).await });
    wait_for_lock(admin.raw(), "shared_cleanup").await?;
    let tenant: TenantId = sqlx::query_scalar("SELECT id FROM tenants LIMIT 1")
        .fetch_one(admin.raw())
        .await?;
    sqlx::query("SELECT set_config('app.tenant_id',$1,true)")
        .bind(tenant.get().to_string())
        .execute(&mut *writer)
        .await?;
    sqlx::query(
        "INSERT INTO tenant_events (tenant_id,canonical_event_id,source_ref)
                 SELECT tenant_id,$1,id FROM source_subscriptions WHERE tenant_id=$2",
    )
    .bind(event)
    .bind(tenant.get())
    .execute(&mut *writer)
    .await?;
    writer.commit().await?;
    assert_eq!(task.await??, Some(0));
    assert_eq!(count(admin.raw()).await?, 1);
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn cleanup_first_prevents_late_fanout_and_late_relay_completion(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, app, admin, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "32 days").await?;
    sqlx::query("INSERT INTO media_relay_jobs (canonical_event_id,receiver_identity_id,status,next_retry_at)
                VALUES ($1,$2,'DOWNLOADING',now()-interval '1 hour')")
        .bind(event).bind(identity).execute(&app).await?;
    let mut held = app.begin().await?;
    sqlx::query("UPDATE canonical_events SET text='private body' WHERE id=$1")
        .bind(event)
        .execute(&mut *held)
        .await?;
    let pool = cleanup.clone();
    let cleaning = tokio::spawn(async move { cleanup_batch(&pool).await });
    wait_for_lock(admin.raw(), "shared_cleanup").await?;
    let tenant: TenantId = sqlx::query_scalar("SELECT id FROM tenants LIMIT 1")
        .fetch_one(admin.raw())
        .await?;
    let pool = app.clone();
    let fanout =
        tokio::spawn(async move { repo::fanout_canonical_event(&pool, tenant, event).await });
    wait_for_lock(admin.raw(), "app_user").await?;
    held.commit().await?;
    assert_eq!(cleaning.await??, Some(1));
    assert!(fanout.await??.is_none());
    let identity: IdentityId = sqlx::query_scalar("SELECT id FROM identities WHERE id=$1")
        .bind(identity)
        .fetch_one(admin.raw())
        .await?;
    let token = repo::acquire_lease(&app, identity, "shared-test")
        .await?
        .ok_or("租约失败")?;
    let lease = repo::LeaseGuard {
        identity_id: identity,
        worker_id: "shared-test".to_owned(),
        fencing_token: token,
    };
    let archive = repo::MediaArchiveInput {
        content_hash: "shared-hash".to_owned(),
        media_type: "PHOTO".to_owned(),
        mime_type: None,
        file_name: None,
        file_size: 1,
        object_key: "staging/test".to_owned(),
    };
    assert!(
        repo::complete_media_relay_job(&app, &lease, event, &archive)
            .await?
            .is_none()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM media_archive")
            .fetch_one(&app)
            .await?,
        0
    );
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn relay_claim_refreshes_before_cleanup_rechecks_its_snapshot(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, app, admin, identity) = setup(injected).await?;
    let event = seed_event(admin.raw(), identity, 1, "32 days").await?;
    sqlx::query("INSERT INTO media_relay_jobs (canonical_event_id,receiver_identity_id,status,next_retry_at)
                VALUES ($1,$2,'DOWNLOADING',now()-interval '1 hour')")
        .bind(event).bind(identity).execute(&app).await?;
    let mut writer = app.begin().await?;
    lock_writer(&mut writer, identity).await?;
    let pool = cleanup.clone();
    let cleaning = tokio::spawn(async move { cleanup_batch(&pool).await });
    wait_for_lock(admin.raw(), "shared_cleanup").await?;
    sqlx::query("UPDATE media_relay_jobs SET next_retry_at=now()+interval '1 hour' WHERE canonical_event_id=$1")
        .bind(event).execute(&mut *writer).await?;
    writer.commit().await?;
    assert_eq!(cleaning.await??, Some(0));
    assert_eq!(count(admin.raw()).await?, 1);
    sqlx::query("UPDATE media_relay_jobs SET status='PENDING',next_retry_at=NULL")
        .execute(&app)
        .await?;
    assert_eq!(cleanup_batch(&cleanup).await?, Some(1));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn late_group_binding_blocks_member_deletion_and_repeatable_read_is_rejected(
    injected: PgPool,
) -> Result<(), Box<dyn std::error::Error>> {
    let (cleanup, app, admin, identity) = setup(injected).await?;
    let member = seed_event(admin.raw(), identity, 1, "32 days").await?;
    let group = seed_event(admin.raw(), identity, 2, "1 hour").await?;
    let mut writer = app.begin().await?;
    lock_writer(&mut writer, identity).await?;
    let pool = cleanup.clone();
    let cleaning = tokio::spawn(async move { cleanup_batch(&pool).await });
    wait_for_lock(admin.raw(), "shared_cleanup").await?;
    sqlx::query("INSERT INTO media_group_members (group_event_id,member_event_id,position) VALUES ($1,$2,0)")
        .bind(group).bind(member).execute(&mut *writer).await?;
    writer.commit().await?;
    assert_eq!(cleaning.await??, Some(0));
    let mut transaction = app.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await?;
    assert!(lock_writer(&mut transaction, identity).await.is_err());
    transaction.rollback().await?;
    let pool = PgPoolOptions::new()
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET default_transaction_isolation='repeatable read'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(cleanup.connect_options().as_ref().clone())
        .await?;
    assert!(cleanup_batch(&pool).await.is_err());
    Ok(())
}
