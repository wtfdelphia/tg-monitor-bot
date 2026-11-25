//! G4 生产历史指纹重算的隔离生命周期测试。
//! 定义处：`docs/plan/proposals/G4-production-fingerprint-readonly-access.md` §五。
//!
//! 形态：`#[sqlx::test]` 注入的临时库充当「已批准恢复副本」（源），
//! 另建一个空临时库充当裁剪目标；扫描角色按 preflight.rs 的同形做法
//! 在常驻库上以 postgres 建立并回收。测试只连临时库，不碰生产库 `tgm`。

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::time::Duration;
use tgm_core::notification::notification_fingerprint;
use tgm_db::g4_fingerprint_scan as scan;

const G4_SCAN_CATALOG_LOCK: i64 = 0x5447_4d47_3453_434e;

/// 以 postgres 连常驻库 —— 只为建/删一次性扫描角色，同 preflight.rs 形态。
async fn superuser_pool() -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!(
            "postgres://postgres:pw@127.0.0.1:{}/tgm",
            support::direct_port()
        ))
        .await
        .expect("以 postgres 连测试 tgm 库失败")
}

struct ScanRole {
    name: String,
}

impl ScanRole {
    async fn create(admin: &PgPool) -> Self {
        let name = format!("g4_sc_{}", std::process::id());
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(G4_SCAN_CATALOG_LOCK)
            .execute(admin)
            .await
            .expect("获取扫描角色目录锁失败");
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                .bind(&name)
                .fetch_one(admin)
                .await
                .expect("检查扫描角色是否存在失败");
        if !exists {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE ROLE {name} LOGIN PASSWORD 'spw' NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION"
            )))
            .execute(admin)
            .await
            .expect("建扫描角色失败");
        }
        Self { name }
    }

    async fn grant_connect(&self, admin: &PgPool, database: &str) {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "GRANT CONNECT ON DATABASE {database} TO {}",
            self.name
        )))
        .execute(admin)
        .await
        .expect("授予扫描角色连接权限失败");
    }

    async fn cleanup(&self, admin: &PgPool) {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                .bind(&self.name)
                .fetch_one(admin)
                .await
                .unwrap_or(false);
        if exists {
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!("DROP ROLE {}", self.name)))
                .execute(admin)
                .await;
        }
    }

    fn url(&self, database: &str) -> String {
        format!(
            "postgres://{}:spw@127.0.0.1:{}/{database}",
            self.name,
            support::direct_port()
        )
    }
}

/// 源（临时库）里灌入四种形态的 `SENT` 记录。返回租户编号。
async fn seed_source(admin: &PgPool) -> i64 {
    let tenant: i64 = sqlx::query_scalar(
        "INSERT INTO tenants (name, owner_user_id) VALUES ('g4-scan-src', 41001) RETURNING id",
    )
    .fetch_one(admin)
    .await
    .expect("建源测试租户失败");
    let identity: i64 = sqlx::query_scalar(
        "INSERT INTO identities (tenant_id, kind, status) VALUES ($1, 'bot', 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin)
    .await
    .expect("建源测试身份失败");

    let mut canonical_ids = Vec::new();
    for (message_id, sender, text) in [
        (9_101_i64, Some(77_i64), Some("scan body")),
        (9_102, Some(77), Some("other body")),
        (9_104, None, None),
    ] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO canonical_events
                (adapter, receiver_identity_id, tg_chat_id, tg_message_id, message_date, sender_id, text)
             VALUES ('bot', $1, -9100, $2, '2026-06-01T10:00:00Z', $3, $4) RETURNING id",
        )
        .bind(identity)
        .bind(message_id)
        .bind(sender)
        .bind(text)
        .fetch_one(admin)
        .await
        .expect("建源规范事件失败");
        canonical_ids.push(id);
    }

    // tenant_events.source_ref 的复合外键要求订阅先存在。
    sqlx::query(
        "INSERT INTO source_subscriptions (tenant_id, tg_chat_id, receiver_identity_id)
         VALUES ($1, -9100, $2)",
    )
    .bind(tenant)
    .bind(identity)
    .execute(admin)
    .await
    .expect("建源订阅失败");

    let mut event_ids = Vec::new();
    for canonical in &canonical_ids {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO tenant_events
                (tenant_id, canonical_event_id, source_ref, matched_keywords, matched, processed_at)
             VALUES ($1, $2, (SELECT id FROM source_subscriptions WHERE tenant_id = $1 LIMIT 1),
                     ARRAY['scan'], TRUE, '2026-06-01T10:30:00Z') RETURNING id",
        )
        .bind(tenant)
        .bind(canonical)
        .fetch_one(admin)
        .await
        .expect("建源租户事件失败");
        event_ids.push(id);
    }

    let correct = notification_fingerprint(Some(77), &["scan".to_owned()], Some("scan body"), None);
    let tasks = [
        (9_201_i64, Some(event_ids[0]), correct.clone(), 501_i64),
        (
            9_202,
            Some(event_ids[1]),
            "deadbeef00000000".to_owned(),
            502,
        ),
        (
            9_204,
            Some(event_ids[2]),
            "cafe000000000000".to_owned(),
            504,
        ),
    ];
    for (task_id, event_id, fingerprint, target) in tasks {
        sqlx::query(
            "INSERT INTO delivery_tasks
                (tenant_id, tenant_event_id, target_tg_chat_id, sender_identity_id,
                 action, status, notification_fingerprint, notification_target_ref,
                 completed_at, created_at)
             VALUES ($1, $2, -9200, $3, 'COPY', 'DONE', $4, $5,
                     '2026-06-01T11:00:00Z', '2026-06-01T10:40:00Z')",
        )
        .bind(tenant)
        .bind(event_id)
        .bind(identity)
        .bind(fingerprint)
        .bind(target)
        .execute(admin)
        .await
        .expect("建源投递任务失败");
        let _ = task_id; // id 由 identity 列生成，关联走指纹列
    }

    let dedup_rows = [
        (correct, 501_i64),
        ("deadbeef00000000".to_owned(), 502),
        ("0000000000000000".to_owned(), 503), // 无对应 DONE 任务
        ("cafe000000000000".to_owned(), 504), // 对应已脱敏事件
    ];
    for (fingerprint, target) in dedup_rows {
        sqlx::query(
            "INSERT INTO notify_dedup
                (tenant_id, fingerprint, target_ref, last_sent_at, expires_at, status)
             VALUES ($1, $2, $3, '2026-06-01T12:00:00Z', '2026-06-02T12:00:00Z', 'SENT')",
        )
        .bind(tenant)
        .bind(fingerprint)
        .bind(target)
        .execute(admin)
        .await
        .expect("建源去重记录失败");
    }
    tenant
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn trim_scan_destroy_lifecycle_only_outputs_aggregates(admin: PgPool) {
    support::assert_role(&admin, "tgm_test").await;
    let source_db: String = sqlx::query("SELECT current_database()")
        .fetch_one(&admin)
        .await
        .expect("取当前库名失败")
        .get(0);
    let port = support::direct_port();
    let tenant = seed_source(&admin).await;

    // 裁剪目标：tgm_test 有 CREATEDB，另建一个空临时库。
    let dest_db = format!("g4_dest_{}", std::process::id());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {dest_db}"
    )))
    .execute(&admin)
    .await
    .expect("清理旧裁剪库失败");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE {dest_db} OWNER tgm_test"
    )))
    .execute(&admin)
    .await
    .expect("建裁剪目标库失败");
    let dest = PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!(
            "postgres://tgm_test:tpw@127.0.0.1:{port}/{dest_db}"
        ))
        .await
        .expect("连裁剪目标库失败");

    let superuser = superuser_pool().await;
    let scan_role = ScanRole::create(&superuser).await;
    scan_role.grant_connect(&superuser, &dest_db).await;

    let scope = scan::ApprovedScope {
        approval_ref: "TEST-G4-001".to_owned(),
        tenants: vec![tenant],
        window_start: None,
        window_end: None,
        as_of: scan::parse_rfc3339("2026-06-02T00:00:00Z").expect("解析失败"),
        include_expired: false,
        allow_content_read: true,
        max_rows: 1_000,
        max_duration_secs: 300,
    };
    let trim = scan::run_trim(&admin, &dest, &scope, &scan_role.name)
        .await
        .expect("裁剪失败");
    assert!(!trim.stopped_by_rows);
    assert_eq!(trim.notify_dedup_rows, 4);
    assert_eq!(trim.delivery_task_rows, 3);
    assert_eq!(trim.tenant_event_rows, 3);
    assert_eq!(trim.canonical_event_rows, 3);

    // 重复裁剪必须被拒绝：目标已带标记。
    let again = scan::run_trim(&admin, &dest, &scope, &scan_role.name).await;
    assert!(
        matches!(again, Err(scan::ScanError::Destination(_))),
        "重复裁剪应被拒绝：{again:?}"
    );

    // 扫描：以临时只读身份连裁剪副本。
    let scan_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&scan_role.url(&dest_db))
        .await
        .expect("以扫描角色连裁剪副本失败");
    let loaded = scan::load_marker(&scan_pool, "TEST-G4-001")
        .await
        .expect("读取范围标记失败");
    assert_eq!(loaded, scope);
    let report = scan::run_scan(&scan_pool, &loaded, Duration::from_secs(300), None)
        .await
        .expect("扫描失败");
    assert_eq!(report.tenants_scanned, 1);
    assert_eq!(report.scanned, 4);
    assert_eq!(report.matched, 1, "正确指纹应重算一致");
    assert_eq!(report.mismatched, 1, "错误存储指纹应计为不一致");
    assert_eq!(report.not_recomputable_no_task, 1);
    assert_eq!(report.not_recomputable_missing_source, 0);
    assert_eq!(
        report.not_recomputable_scrubbed, 1,
        "脱敏事件应计为不可复算"
    );
    assert!(!report.stopped_by_rows);
    assert!(!report.stopped_by_duration);

    // 行数上限必须只收紧：请求超过标记值时以标记值为准。
    let capped = scan::run_scan(&scan_pool, &loaded, Duration::from_secs(300), Some(2))
        .await
        .expect("限量扫描失败");
    assert_eq!(capped.scanned, 2);
    assert!(capped.stopped_by_rows);

    // 生产角色直接扫描必须被拒（postgres 是超级用户）。
    let super_scan = scan::run_scan(&superuser, &loaded, Duration::from_secs(300), None).await;
    assert!(
        matches!(super_scan, Err(scan::ScanError::Identity(_))),
        "超级用户扫描应被拒：{super_scan:?}"
    );

    // 未批准内容读取的范围不得重算。
    let mut denied = loaded.clone();
    denied.allow_content_read = false;
    let refused = scan::run_scan(&scan_pool, &denied, Duration::from_secs(300), None).await;
    assert!(matches!(refused, Err(scan::ScanError::ContentNotApproved)));

    // 销毁：标记存在才允许，且必须由建表的装载角色执行
    // （DROP TABLE 要所有权 —— 扫描角色拿不到写权限是设计意图）。
    // 销毁后标记缺失，二次销毁被拒。
    let dropped = scan::run_destroy(&dest).await.expect("销毁失败");
    assert_eq!(dropped.len(), 5);
    assert!(matches!(
        scan::run_destroy(&dest).await,
        Err(scan::ScanError::MarkerMissing)
    ));
    // 扫描角色无所有权，销毁必须失败。
    assert!(scan::run_destroy(&scan_pool).await.is_err());

    scan_pool.close().await;
    dest.close().await;
    scan_role.cleanup(&superuser).await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {dest_db}"
    )))
    .execute(&admin)
    .await
    .expect("清理裁剪库失败");
    let _ = source_db;
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn scan_and_destroy_refuse_databases_without_the_marker(admin: PgPool) {
    support::assert_role(&admin, "tgm_test").await;
    // 本临时库没有 g4_scan_scope —— 与生产库同形。
    assert!(matches!(
        scan::load_marker(&admin, "ANY").await,
        Err(scan::ScanError::MarkerMissing)
    ));
    assert!(matches!(
        scan::run_destroy(&admin).await,
        Err(scan::ScanError::MarkerMissing)
    ));
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn trim_refuses_a_destination_that_already_holds_rows(admin: PgPool) {
    support::assert_role(&admin, "tgm_test").await;
    // 用源临时库自己当目标：它有真实业务数据（且无标记），必须被拒。
    let tenant = seed_source(&admin).await;
    let scope = scan::ApprovedScope {
        approval_ref: "TEST-G4-002".to_owned(),
        tenants: vec![tenant],
        window_start: None,
        window_end: None,
        as_of: scan::parse_rfc3339("2026-06-02T00:00:00Z").expect("解析失败"),
        include_expired: false,
        allow_content_read: true,
        max_rows: 10,
        max_duration_secs: 60,
    };
    let result = scan::run_trim(&admin, &admin, &scope, "g4_scan_ro").await;
    assert!(
        matches!(result, Err(scan::ScanError::Destination(_))),
        "已有数据的目标库应被拒：{result:?}"
    );
}
