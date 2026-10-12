#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use std::time::Duration;
use tgm_db::repo::{
    ConsoleLoginRepoError, WebSessionLoginMethod, claim_console_login_challenge,
    confirm_console_login_challenge, create_console_login_challenge,
    expire_console_login_challenge, issue_web_session_for_user, load_console_login_challenge,
    reject_console_login_challenge,
};

#[sqlx::test(migrations = "../../migrations-test")]
async fn web_session_management_migration_preserves_legacy_sessions(admin: PgPool) {
    let (_app, admin) = support::pools(admin).await;
    let mut tx = admin.raw().begin().await.expect("开启历史会话迁移事务失败");
    sqlx::raw_sql(
        "SET ROLE tgm_owner; ALTER TABLE web_sessions DROP COLUMN login_method; RESET ROLE;",
    )
    .execute(&mut *tx)
    .await
    .expect("恢复历史会话字段形状失败");
    let tenant: i64 = sqlx::query_scalar(
        "INSERT INTO tenants (name, owner_user_id) VALUES ('legacy-session', 7451) RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("创建历史会话租户失败");
    sqlx::query("INSERT INTO tenant_members (tenant_id, user_id) VALUES ($1, 7451)")
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .expect("创建历史会话成员失败");
    let session_id = [41_u8; 32];
    let csrf = [42_u8; 32];
    sqlx::query("INSERT INTO web_sessions (tenant_id, id, user_id, csrf_token, expires_at) VALUES ($1, $2, 7451, $3, now() + interval '30 days')")
        .bind(tenant).bind(session_id.as_slice()).bind(csrf.as_slice())
        .execute(&mut *tx).await.expect("创建迁移前会话失败");
    for _ in 0..2 {
        sqlx::raw_sql(include_str!(
            "../../../migrations/0017_web_session_management.sql"
        ))
        .execute(&mut *tx)
        .await
        .expect("历史会话迁移或幂等重跑失败");
    }
    let row: (String, Vec<u8>) =
        sqlx::query_as("SELECT login_method, csrf_token FROM web_sessions WHERE id = $1")
            .bind(session_id.as_slice())
            .fetch_one(&mut *tx)
            .await
            .expect("读取迁移后历史会话失败");
    assert_eq!(row, ("unknown".to_owned(), csrf.to_vec()));
    tx.commit().await.expect("提交历史会话迁移失败");
    let err = sqlx::query("UPDATE web_sessions SET login_method = 'guessed' WHERE id = $1")
        .bind(session_id.as_slice())
        .execute(admin.raw())
        .await
        .expect_err("登录方式约束接受了未知值");
    assert_eq!(
        err.as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn console_login_challenge_claim_and_confirm_are_single_use(admin: PgPool) {
    let (_app, admin) = support::pools(admin).await;
    let auth = support::auth_pool(&admin).await;
    let nonce = vec![21_u8; 32];
    let expires_at = Utc::now() + Duration::from_secs(120);

    create_console_login_challenge(
        &auth,
        &nonce,
        "A7K3",
        "127.0.0.1",
        Some("test-agent"),
        expires_at,
    )
    .await
    .expect("创建控制台登录挑战失败");

    assert!(
        claim_console_login_challenge(&auth, &nonce, 7001)
            .await
            .expect("第一次扫码应成功")
    );
    assert!(
        !claim_console_login_challenge(&auth, &nonce, 7002)
            .await
            .expect("第二次扫码应返回 false")
    );
    assert!(
        !confirm_console_login_challenge(&auth, &nonce, 7001, "A7K4")
            .await
            .expect("错误确认码应返回 false")
    );
    assert!(
        !confirm_console_login_challenge(&auth, &nonce, 7001, "A7K3")
            .await
            .expect("错误确认后正确码也应返回 false")
    );
    assert!(
        !reject_console_login_challenge(&auth, &nonce, 7001)
            .await
            .expect("拒绝终态应返回 false")
    );

    let record = load_console_login_challenge(&auth, &nonce)
        .await
        .expect("读取挑战失败")
        .expect("挑战行应存在");
    assert_eq!(record.state, "rejected");
    assert_eq!(record.tg_user_id, Some(7001));
    assert_eq!(record.origin_ip, "127.0.0.1/32");
    assert_eq!(record.user_agent.as_deref(), Some("test-agent"));
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn console_login_repo_rejects_invalid_inputs_and_duplicate_pending_actor(admin: PgPool) {
    let (_app, admin) = support::pools(admin).await;
    let auth = support::auth_pool(&admin).await;
    let invalid_nonce = vec![1_u8; 31];

    let error = create_console_login_challenge(
        &auth,
        &invalid_nonce,
        "A7K3",
        "127.0.0.1",
        None,
        Utc::now() + Duration::from_secs(120),
    )
    .await
    .expect_err("短 nonce 竟然被接受");
    assert!(matches!(error, ConsoleLoginRepoError::InvalidNonce));

    let nonce_one = vec![31_u8; 32];
    let nonce_two = vec![32_u8; 32];
    for nonce in [&nonce_one, &nonce_two] {
        create_console_login_challenge(
            &auth,
            nonce,
            "C9P2",
            "127.0.0.1",
            None,
            Utc::now() + Duration::from_secs(120),
        )
        .await
        .expect("创建重复账号测试挑战失败");
    }

    assert!(matches!(
        claim_console_login_challenge(&auth, &nonce_one, 0)
            .await
            .expect_err("非法 Telegram 用户 id 竟然被接受"),
        ConsoleLoginRepoError::InvalidTelegramUserId
    ));
    assert!(
        claim_console_login_challenge(&auth, &nonce_one, 9001)
            .await
            .expect("首个账号应答失败")
    );
    assert!(
        !claim_console_login_challenge(&auth, &nonce_two, 9001)
            .await
            .expect("同一账号的第二个待确认挑战应返回 false")
    );
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn expired_console_login_challenge_cannot_be_claimed(admin: PgPool) {
    let (_app, admin) = support::pools(admin).await;
    let auth = support::auth_pool(&admin).await;
    let nonce = vec![22_u8; 32];

    create_console_login_challenge(
        &auth,
        &nonce,
        "B8M4",
        "127.0.0.1",
        None,
        Utc::now() - Duration::from_secs(1),
    )
    .await
    .expect("创建过期挑战失败");

    assert!(
        !claim_console_login_challenge(&auth, &nonce, 7001)
            .await
            .expect("过期挑战 claim 应返回 false")
    );
    assert!(
        expire_console_login_challenge(&auth, &nonce)
            .await
            .expect("过期挑战应可标记 expired")
    );
    let record = load_console_login_challenge(&auth, &nonce)
        .await
        .expect("读取过期挑战失败")
        .expect("过期挑战行应存在");
    assert_eq!(record.state, "expired");
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn web_session_issue_derives_one_tenant_and_rejects_ambiguous_users(admin: PgPool) {
    let (_app, admin) = support::pools(admin).await;
    let auth = support::auth_pool(&admin).await;
    let tenant_a: i64 = sqlx::query_scalar(
        "INSERT INTO tenants (name, owner_user_id) VALUES ('session-issue-a', 7401) RETURNING id",
    )
    .fetch_one(admin.raw())
    .await
    .expect("创建唯一租户失败");
    let tenant_b: i64 = sqlx::query_scalar(
        "INSERT INTO tenants (name, owner_user_id) VALUES ('session-issue-b', 7402) RETURNING id",
    )
    .fetch_one(admin.raw())
    .await
    .expect("创建第二租户失败");
    const UNIQUE_USER: i64 = 7401;
    const AMBIGUOUS_USER: i64 = 7403;

    sqlx::query(
        "INSERT INTO tenant_members (tenant_id, user_id, role)
         VALUES ($1, $2, 'TENANT_OWNER'), ($3, $4, 'TENANT_OWNER'), ($5, $4, 'TENANT_OWNER')",
    )
    .bind(tenant_a)
    .bind(UNIQUE_USER)
    .bind(tenant_a)
    .bind(AMBIGUOUS_USER)
    .bind(tenant_b)
    .execute(admin.raw())
    .await
    .expect("创建会话签发成员失败");

    let issue = issue_web_session_for_user(
        &auth,
        UNIQUE_USER,
        "203.0.113.40",
        Some("oidc-or-bot-test"),
        WebSessionLoginMethod::Oidc,
    )
    .await
    .expect("唯一租户用户签发会话失败")
    .expect("唯一租户用户没有拿到会话");
    assert_eq!(issue.session_id.len(), 32);
    assert_eq!(issue.csrf_token.len(), 32);

    let row: (i64, i64, String, Option<String>, String) = sqlx::query_as(
        "SELECT tenant_id, user_id, ip::text, user_agent, login_method
           FROM web_sessions
          WHERE id = $1",
    )
    .bind(&issue.session_id)
    .fetch_one(admin.raw())
    .await
    .expect("读取会话签发行失败");
    assert_eq!(
        row,
        (
            tenant_a,
            UNIQUE_USER,
            "203.0.113.40/32".to_owned(),
            Some("oidc-or-bot-test".to_owned()),
            "oidc".to_owned(),
        )
    );

    let ambiguous = issue_web_session_for_user(
        &auth,
        AMBIGUOUS_USER,
        "203.0.113.41",
        Some("ambiguous"),
        WebSessionLoginMethod::Oidc,
    )
    .await
    .expect("多租户用户签发会话查询失败");
    assert!(ambiguous.is_none(), "多租户用户不应被猜租户");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM web_sessions WHERE user_id = $1")
        .bind(AMBIGUOUS_USER)
        .fetch_one(admin.raw())
        .await
        .expect("统计多租户用户会话失败");
    assert_eq!(count, 0);
}
