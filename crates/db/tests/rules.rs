//! 规则 Repo 的 L2 测试。行为定义见 docs/design/spec/03-数据模型.md §4.1–§4.2。

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use sqlx::PgPool;
use tgm_core::error::AppError;
use tgm_core::ids::{IdentityId, TenantId};
use tgm_db::repo::{
    RuleConditionInput, RuleCreateInput, RuleReplacementInput, RuleUpdateInput, create_rule,
    update_rule,
};

async fn seed_tenant(admin: &support::AdminOnlyPool, name: &str) -> TenantId {
    sqlx::query_scalar::<_, TenantId>(
        "INSERT INTO tenants (name, owner_user_id) VALUES ($1, $2) RETURNING id",
    )
    .bind(name)
    .bind(if name == "rules-tenant-a" {
        3001_i64
    } else {
        3002_i64
    })
    .fetch_one(admin.raw())
    .await
    .expect("创建规则测试租户失败")
}

async fn seed_identity(admin: &support::AdminOnlyPool, tenant: TenantId) -> IdentityId {
    sqlx::query_scalar::<_, IdentityId>(
        "INSERT INTO identities (kind, tenant_id, status) VALUES ('bot', $1, 'offline') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则测试身份失败")
}

async fn seed_rule_dependencies(
    admin: &support::AdminOnlyPool,
    tenant: TenantId,
) -> (i64, i64, i64) {
    let identity = seed_identity(admin, tenant).await;
    let target_ref: i64 = sqlx::query_scalar(
        "INSERT INTO target_channels (tenant_id, tg_chat_id, sender_identity_id, enabled)
         VALUES ($1, $2, $3, TRUE) RETURNING id",
    )
    .bind(tenant)
    .bind(-20_000 - tenant.get())
    .bind(identity)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则测试目标失败");
    let keyword_id: i64 = sqlx::query_scalar(
        "INSERT INTO keywords (tenant_id, word, match_type, enabled)
         VALUES ($1, 'rules-keyword', 'exact', TRUE) RETURNING id",
    )
    .bind(tenant)
    .fetch_one(admin.raw())
    .await
    .expect("创建规则测试关键词失败");
    (keyword_id, target_ref, identity.get())
}

fn input(keyword_id: i64, target_ref: i64) -> RuleCreateInput {
    RuleCreateInput {
        keyword_id,
        source_ref: None,
        media_type: Some("TEXT".to_owned()),
        target_ref,
        action: "COPY".to_owned(),
        priority: 10,
        stop_processing: false,
        template: Some("header".to_owned()),
        enabled: true,
        conditions: vec![RuleConditionInput {
            kind: "sender".to_owned(),
            value: "7".to_owned(),
        }],
        replacements: vec![RuleReplacementInput {
            seq: 1,
            pattern: "urgent".to_owned(),
            replacement: "URGENT".to_owned(),
            enabled: true,
        }],
    }
}

fn update_from_create(input: &RuleCreateInput) -> RuleUpdateInput {
    RuleUpdateInput {
        keyword_id: input.keyword_id,
        source_ref: input.source_ref,
        media_type: input.media_type.clone(),
        target_ref: input.target_ref,
        action: input.action.clone(),
        priority: input.priority,
        stop_processing: input.stop_processing,
        template: input.template.clone(),
        enabled: input.enabled,
        conditions: input.conditions.clone(),
        replacements: input.replacements.clone(),
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn create_rule_maps_conflict_and_writes_initial_snapshot(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "rules-tenant-a").await;
    let (keyword_id, target_ref, _) = seed_rule_dependencies(&admin, tenant).await;
    let rule_input = input(keyword_id, target_ref);

    let first = create_rule(&app, tenant, rule_input.clone())
        .await
        .expect("创建规则失败");
    assert_eq!(first.version, 1);
    assert!(first.semantic_changed);

    let snapshot: serde_json::Value = sqlx::query_scalar(
        "SELECT snapshot FROM rule_versions WHERE tenant_id = $1 AND rule_id = $2 AND version = 1",
    )
    .bind(tenant)
    .bind(first.id)
    .fetch_one(admin.raw())
    .await
    .expect("读取初始规则快照失败");
    assert_eq!(snapshot["rule"]["action"], "COPY");
    assert_eq!(snapshot["conditions"][0]["kind"], "sender");
    assert_eq!(snapshot["replacements"][0]["seq"], 1);

    match create_rule(&app, tenant, rule_input).await {
        Err(AppError::RuleConflict { existing_id }) => assert_eq!(existing_id, first.id),
        other => panic!("重复规则未映射为 RuleConflict：{other:?}"),
    }
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn update_rule_only_versions_semantic_changes(admin: PgPool) {
    let (app, admin) = support::pools(admin).await;
    support::assert_role(&app, "app_user").await;
    let tenant = seed_tenant(&admin, "rules-tenant-a").await;
    let (keyword_id, target_ref, _) = seed_rule_dependencies(&admin, tenant).await;
    let created_input = input(keyword_id, target_ref);
    let created = create_rule(&app, tenant, created_input.clone())
        .await
        .expect("创建版本测试规则失败");

    let mut priority_only = update_from_create(&created_input);
    priority_only.priority = 99;
    priority_only.enabled = false;
    let priority_update = update_rule(&app, tenant, created.id, priority_only)
        .await
        .expect("priority/enabled 更新失败");
    assert_eq!(priority_update.version, 1);
    assert!(!priority_update.semantic_changed);

    let version_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rule_versions WHERE tenant_id = $1 AND rule_id = $2",
    )
    .bind(tenant)
    .bind(created.id)
    .fetch_one(admin.raw())
    .await
    .expect("核对非语义更新版本数失败");
    assert_eq!(version_count, 1);

    let mut semantic_update = update_from_create(&created_input);
    semantic_update.action = "FORWARD".to_owned();
    semantic_update.conditions = vec![RuleConditionInput {
        kind: "sender".to_owned(),
        value: "8".to_owned(),
    }];
    let semantic = update_rule(&app, tenant, created.id, semantic_update)
        .await
        .expect("语义更新失败");
    assert_eq!(semantic.version, 2);
    assert!(semantic.semantic_changed);

    let snapshot_action: String = sqlx::query_scalar(
        "SELECT snapshot->'rule'->>'action'
           FROM rule_versions
          WHERE tenant_id = $1 AND rule_id = $2 AND version = 2",
    )
    .bind(tenant)
    .bind(created.id)
    .fetch_one(admin.raw())
    .await
    .expect("核对语义更新快照失败");
    assert_eq!(snapshot_action, "FORWARD");

    let total_versions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rule_versions WHERE tenant_id = $1 AND rule_id = $2",
    )
    .bind(tenant)
    .bind(created.id)
    .fetch_one(admin.raw())
    .await
    .expect("核对版本总数失败");
    assert_eq!(total_versions, 2);
}
