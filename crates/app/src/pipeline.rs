use std::collections::HashMap;

use sqlx::PgPool;
use std::time::Duration;
use tgm_core::error::AppError;
use tgm_core::ids::TenantId;
use tgm_db::repo::{
    DeliveryTaskInput, DeliveryTaskWrite, EnqueueError, RuleConditionRecord, RuleRecord,
    TenantEventRecord, aggregate_ready_media_groups, enqueue_delivery_tasks,
    fanout_canonical_event, list_all_active_tenants, list_unfanned_canonical_event_ids,
    list_unprocessed_tenant_event_ids, load_enabled_rules, load_media_group_members,
    load_rule_version, load_tenant_event, load_tenant_plan, mark_canonical_event_fanned_out,
};
use tokio::sync::watch;

const MIN_WORKER_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_WORKER_RETRY_DELAY: Duration = Duration::from_secs(30);

use crate::rule::{
    KeywordMatchType, Rule, RuleAction, RuleCondition, RuleConditionKind, RuleEvent, evaluate_rules,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanoutWorkerConfig {
    pub poll_interval: Duration,
    pub event_batch_size: i64,
    /// 媒体对象清理的间隔（片 3 批准记录第 6 条：跑在 fanout 里，spec/04 §一「每日」之内）。
    pub cleanup_interval: Duration,
}

impl Default for FanoutWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            event_batch_size: 100,
            cleanup_interval: Duration::from_secs(60 * 60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FanoutRunOutcome {
    pub event_count: usize,
    pub tenant_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanoutWorkerExit {
    Shutdown,
}

pub struct FanoutWorker {
    pool: PgPool,
    config: FanoutWorkerConfig,
    media_store: Option<crate::media_relay::MediaStore>,
    media_cleanup_pool: Option<PgPool>,
}

impl FanoutWorker {
    pub fn new(pool: PgPool, config: FanoutWorkerConfig) -> Self {
        Self {
            pool,
            config,
            media_store: None,
            media_cleanup_pool: None,
        }
    }

    /// 带上对象存储：每 `cleanup_interval` 跑一轮媒体对象清理。
    pub fn with_media_cleanup(
        mut self,
        store: crate::media_relay::MediaStore,
        cleanup_pool: PgPool,
    ) -> Self {
        self.media_store = Some(store);
        self.media_cleanup_pool = Some(cleanup_pool);
        self
    }

    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<FanoutWorkerExit, AppError> {
        let mut retry_delay = self.config.poll_interval;
        // 启动后先跑一轮：进程频繁重启时不至于一直轮不到清理。
        let mut next_cleanup = tokio::time::Instant::now();
        loop {
            if *shutdown.borrow() {
                return Ok(FanoutWorkerExit::Shutdown);
            }
            if let (Some(store), Some(cleanup_pool)) = (&self.media_store, &self.media_cleanup_pool)
                && tokio::time::Instant::now() >= next_cleanup
            {
                next_cleanup = tokio::time::Instant::now() + self.config.cleanup_interval;
                // 清理失败不影响扇出：记日志，下一个间隔再试。
                match crate::media_cleanup::run_cleanup_once(
                    cleanup_pool,
                    store,
                    crate::media_cleanup::default_staging_max_age(),
                )
                .await
                {
                    Ok(outcome) => tracing::info!(?outcome, "媒体对象清理完成"),
                    Err(error) => tracing::warn!(error = %error, "媒体对象清理失败，下个间隔再试"),
                }
            }

            let outcome = match run_fanout_once(&self.pool, self.config.event_batch_size).await {
                Ok(outcome) => {
                    retry_delay = self.config.poll_interval;
                    outcome
                }
                Err(error) if is_retryable_worker_error(&error) => {
                    tracing::warn!(
                        error = %error,
                        retry_after_ms = retry_delay.as_millis(),
                        "fanout worker 数据库操作失败，将退避重试"
                    );
                    if !wait_for_worker_retry(&mut shutdown, retry_delay).await {
                        return Ok(FanoutWorkerExit::Shutdown);
                    }
                    retry_delay = next_worker_retry_delay(retry_delay, self.config.poll_interval);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if outcome.event_count > 0 {
                continue;
            }

            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(FanoutWorkerExit::Shutdown);
                    }
                }
                _ = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
    }
}

pub async fn run_fanout_once(
    pool: &PgPool,
    event_batch_size: i64,
) -> Result<FanoutRunOutcome, AppError> {
    if event_batch_size <= 0 {
        return Err(AppError::ConditionInvalid(
            "worker 批大小必须为正数".to_owned(),
        ));
    }

    aggregate_ready_media_groups(pool, event_batch_size).await?;
    let event_ids = list_unfanned_canonical_event_ids(pool, event_batch_size).await?;
    let mut tenant_count = 0;
    for event_id in &event_ids {
        let tenants = list_all_active_tenants(pool).await?;
        tenant_count += tenants.len();
        for tenant in tenants {
            fanout_canonical_event(pool, tenant, *event_id).await?;
        }
        mark_canonical_event_fanned_out(pool, *event_id).await?;
    }

    Ok(FanoutRunOutcome {
        event_count: event_ids.len(),
        tenant_count,
    })
}

#[derive(Debug, Clone)]
pub struct RuleProcessingResult {
    pub processed: bool,
    pub matched_rule_ids: Vec<i64>,
    pub tasks: Vec<DeliveryTaskWrite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleWorkerConfig {
    pub poll_interval: Duration,
    pub event_batch_size: i64,
}

impl Default for RuleWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            event_batch_size: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleRunOutcome {
    pub tenant_count: usize,
    pub event_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleWorkerExit {
    Shutdown,
}

pub struct RuleWorker {
    pool: PgPool,
    config: RuleWorkerConfig,
}

impl RuleWorker {
    pub fn new(pool: PgPool, config: RuleWorkerConfig) -> Self {
        Self { pool, config }
    }

    pub async fn run(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<RuleWorkerExit, AppError> {
        let mut retry_delay = self.config.poll_interval;
        loop {
            if *shutdown.borrow() {
                return Ok(RuleWorkerExit::Shutdown);
            }

            let outcome = match run_rule_once(&self.pool, self.config.event_batch_size).await {
                Ok(outcome) => {
                    retry_delay = self.config.poll_interval;
                    outcome
                }
                Err(error) if is_retryable_worker_error(&error) => {
                    tracing::warn!(
                        error = %error,
                        retry_after_ms = retry_delay.as_millis(),
                        "rule worker 操作失败，将退避重试"
                    );
                    if !wait_for_worker_retry(&mut shutdown, retry_delay).await {
                        return Ok(RuleWorkerExit::Shutdown);
                    }
                    retry_delay = next_worker_retry_delay(retry_delay, self.config.poll_interval);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if outcome.event_count > 0 {
                continue;
            }

            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(RuleWorkerExit::Shutdown);
                    }
                }
                _ = tokio::time::sleep(self.config.poll_interval) => {}
            }
        }
    }
}

fn is_retryable_worker_error(error: &AppError) -> bool {
    matches!(error, AppError::Db(_))
        || matches!(error, AppError::Other(source) if source.is::<StaleRulePlan>())
}

#[derive(Debug, thiserror::Error)]
#[error("规则计划已失效，需要重新读取规则")]
struct StaleRulePlan;

#[derive(Debug, thiserror::Error)]
#[error("当前规则版本缺少快照")]
struct MissingCurrentRuleSnapshot;

async fn unavailable_rule_version(
    pool: &PgPool,
    tenant: TenantId,
    rule_id: i64,
    version: i32,
) -> Result<AppError, AppError> {
    let rule = tgm_db::repo::load_rule_detail(pool, tenant, rule_id).await?;
    if let Some((rule, _, _)) = rule
        && rule.version == version
    {
        return Ok(AppError::internal(MissingCurrentRuleSnapshot));
    }
    Ok(AppError::internal(StaleRulePlan))
}

fn next_worker_retry_delay(current: Duration, poll_interval: Duration) -> Duration {
    let base = poll_interval.max(MIN_WORKER_RETRY_DELAY);
    let current = current.max(base);
    match current.checked_mul(2) {
        Some(next) => next.min(MAX_WORKER_RETRY_DELAY),
        None => MAX_WORKER_RETRY_DELAY,
    }
}

async fn wait_for_worker_retry(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        changed = shutdown.changed() => changed.is_ok() && !*shutdown.borrow(),
        _ = tokio::time::sleep(delay) => true,
    }
}

pub async fn run_rule_once(
    pool: &PgPool,
    event_batch_size: i64,
) -> Result<RuleRunOutcome, AppError> {
    if event_batch_size <= 0 {
        return Err(AppError::ConditionInvalid(
            "worker 批大小必须为正数".to_owned(),
        ));
    }

    let tenants = list_all_active_tenants(pool).await?;
    let tenant_count = tenants.len();
    let mut event_count = 0;
    for tenant in tenants {
        let event_ids = list_unprocessed_tenant_event_ids(pool, tenant, event_batch_size).await?;
        for event_id in event_ids {
            match process_tenant_event(pool, tenant, event_id).await {
                Err(AppError::NotFound)
                    if load_tenant_event(pool, tenant, event_id).await?.is_none() => {}
                result => {
                    result?;
                }
            }
            event_count += 1;
        }
    }

    Ok(RuleRunOutcome {
        tenant_count,
        event_count,
    })
}

pub async fn process_tenant_event(
    pool: &PgPool,
    tenant: TenantId,
    tenant_event_id: i64,
) -> Result<RuleProcessingResult, AppError> {
    let Some(event_record) = load_tenant_event(pool, tenant, tenant_event_id).await? else {
        return Err(AppError::NotFound);
    };
    if event_record.processed {
        return Ok(RuleProcessingResult {
            processed: false,
            matched_rule_ids: Vec::new(),
            tasks: Vec::new(),
        });
    }

    let rules_with_conditions = load_enabled_rules(pool, tenant).await?;
    let plan = load_tenant_plan(pool, tenant).await?;
    let event = to_rule_event(&event_record);
    let mut rules = Vec::with_capacity(rules_with_conditions.len());
    let mut raw_rules = HashMap::with_capacity(rules_with_conditions.len());
    for (record, conditions) in rules_with_conditions {
        let rule = to_rule(record.clone(), conditions)?;
        raw_rules.insert(rule.id, record);
        rules.push(rule);
    }

    let members = load_media_group_members(pool, tenant, tenant_event_id).await?;
    let matches = if matches!(
        event_record.event_type.as_str(),
        "MESSAGE_CREATED" | "MEDIA_GROUP_COMPLETED"
    ) {
        evaluate_rules(&event, &rules)?
    } else {
        Vec::new()
    };

    let matched_keywords: Vec<String> = matches
        .iter()
        .map(|matched| matched.matched_keyword.clone())
        .collect();
    let mut tasks = Vec::with_capacity(matches.len());
    for matched in &matches {
        let record = raw_rules.get(&matched.rule_id).ok_or(AppError::NotFound)?;
        let Some(version) =
            load_rule_version(pool, tenant, matched.rule_id, matched.rule_version).await?
        else {
            return Err(unavailable_rule_version(
                pool,
                tenant,
                matched.rule_id,
                matched.rule_version,
            )
            .await?);
        };
        let media = media_route(
            &plan,
            matched.action,
            event_record.media_type.as_deref(),
            event_record.file_size,
        );
        tasks.push(DeliveryTaskInput {
            rule_id: matched.rule_id,
            rule_version: matched.rule_version,
            target_tg_chat_id: record.target_tg_chat_id,
            sender_identity_id: record.sender_identity_id,
            action: action_as_str(matched.action).to_string(),
            payload_snapshot: crate::delivery::freeze_delivery_snapshot(
                &event_record,
                &version.snapshot,
                &matched_keywords,
                media == MediaRoute::TextOnly,
                &members,
            )?,
            media_relay: media == MediaRoute::Relay,
            dead_error_code: (media == MediaRoute::TooLarge).then(|| "MEDIA_TOO_LARGE".to_owned()),
        });
    }
    let writes = match enqueue_delivery_tasks(
        pool,
        tenant,
        tenant_event_id,
        &matched_keywords,
        &tasks,
    )
    .await
    {
        Ok(writes) => writes,
        Err(EnqueueError::EventUnavailable | EnqueueError::EventProcessed) => {
            return Ok(RuleProcessingResult {
                processed: false,
                matched_rule_ids: Vec::new(),
                tasks: Vec::new(),
            });
        }
        Err(EnqueueError::Database(error)) => return Err(error.into()),
        Err(EnqueueError::VersionUnavailable { rule_id, version }) => {
            return Err(unavailable_rule_version(pool, tenant, rule_id, version).await?);
        }
    };

    Ok(RuleProcessingResult {
        processed: true,
        matched_rule_ids: matches.iter().map(|matched| matched.rule_id).collect(),
        tasks: writes,
    })
}

/// 一条命中任务的媒体怎么走（片 2 批准记录第 1、5 条与 free 档口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaRoute {
    /// 文字消息或 FORWARD：沿用原路径。FORWARD 读不到源消息仍判永久失败，不回落到中转。
    Direct,
    /// free 档 COPY / RESEND 的媒体：只发文字。
    TextOnly,
    /// pro 档 COPY / RESEND 的媒体：跨身份中转。
    Relay,
    /// pro 档但已知超过单文件上限：入队即判死。
    TooLarge,
}

pub fn media_route(
    plan: &str,
    action: RuleAction,
    media_type: Option<&str>,
    file_size: Option<i64>,
) -> MediaRoute {
    let is_media = media_type.is_some_and(|media_type| media_type != "TEXT");
    if !is_media || action == RuleAction::Forward {
        return MediaRoute::Direct;
    }
    if plan != "pro" {
        return MediaRoute::TextOnly;
    }
    if file_size.is_some_and(|size| size > crate::media_relay::MEDIA_RELAY_MAX_BYTES) {
        return MediaRoute::TooLarge;
    }
    MediaRoute::Relay
}

fn to_rule_event(record: &TenantEventRecord) -> RuleEvent {
    RuleEvent {
        source_ref: record.source_ref,
        media_type: record.media_type.clone(),
        sender_id: record.sender_id,
        text: record.text.clone(),
        caption: record.caption.clone(),
        file_name: record.file_name.clone(),
        file_size: record.file_size,
    }
}

pub(crate) fn to_rule(
    record: RuleRecord,
    conditions: Vec<RuleConditionRecord>,
) -> Result<Rule, AppError> {
    Ok(Rule {
        id: record.id,
        tenant_id: record.tenant_id,
        keyword: crate::rule::RuleKeyword {
            id: record.keyword_id,
            word: record.keyword,
            match_type: match record.keyword_match_type.as_str() {
                "exact" => KeywordMatchType::Exact,
                "regex" => KeywordMatchType::Regex,
                other => {
                    return Err(AppError::ConditionInvalid(format!(
                        "暂不支持关键词匹配类型: {other}"
                    )));
                }
            },
        },
        source_ref: record.source_ref,
        media_type: record.media_type,
        target_ref: record.target_ref,
        action: parse_action(&record.action)?,
        priority: record.priority,
        stop_processing: record.stop_processing,
        version: record.version,
        enabled: record.enabled,
        conditions: conditions
            .into_iter()
            .map(parse_condition)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn parse_action(value: &str) -> Result<RuleAction, AppError> {
    match value {
        "COPY" => Ok(RuleAction::Copy),
        "FORWARD" => Ok(RuleAction::Forward),
        "RESEND" => Ok(RuleAction::Resend),
        other => Err(AppError::ConditionInvalid(format!("未知投递动作: {other}"))),
    }
}

fn parse_condition(record: RuleConditionRecord) -> Result<RuleCondition, AppError> {
    let kind = match record.kind.as_str() {
        "exclude_word" => RuleConditionKind::ExcludeWord,
        "file_name" => RuleConditionKind::FileName,
        "size_min" => RuleConditionKind::SizeMin,
        "size_max" => RuleConditionKind::SizeMax,
        "sender" => RuleConditionKind::Sender,
        "ext_allow" => RuleConditionKind::ExtAllow,
        "ext_deny" => RuleConditionKind::ExtDeny,
        other => return Err(AppError::ConditionInvalid(format!("未知规则条件: {other}"))),
    };
    Ok(RuleCondition {
        kind,
        value: record.value,
    })
}

fn action_as_str(action: RuleAction) -> &'static str {
    match action {
        RuleAction::Copy => "COPY",
        RuleAction::Forward => "FORWARD",
        RuleAction::Resend => "RESEND",
    }
}

#[cfg(test)]
mod worker_tests {
    use super::{MAX_WORKER_RETRY_DELAY, MIN_WORKER_RETRY_DELAY, next_worker_retry_delay};
    use std::time::Duration;

    #[test]
    fn stale_plans_retry_but_missing_current_snapshots_and_not_found_do_not() {
        assert!(super::is_retryable_worker_error(
            &super::AppError::internal(super::StaleRulePlan)
        ));
        assert!(!super::is_retryable_worker_error(
            &super::AppError::internal(super::MissingCurrentRuleSnapshot)
        ));
        assert!(!super::is_retryable_worker_error(
            &super::AppError::NotFound
        ));
    }
    #[test]
    fn retry_delay_starts_at_poll_interval_and_caps() {
        assert_eq!(
            next_worker_retry_delay(Duration::from_millis(1), Duration::from_secs(2)),
            Duration::from_secs(4)
        );
        assert_eq!(
            next_worker_retry_delay(Duration::from_millis(1), Duration::ZERO),
            MIN_WORKER_RETRY_DELAY * 2
        );
        assert_eq!(
            next_worker_retry_delay(MAX_WORKER_RETRY_DELAY, Duration::from_secs(1)),
            MAX_WORKER_RETRY_DELAY
        );
    }
}

#[cfg(test)]
mod media_route_tests {
    use super::{MediaRoute, media_route};
    use crate::rule::RuleAction;

    #[test]
    fn routes_follow_plan_action_and_size() {
        const LIMIT: i64 = 50 * 1024 * 1024;
        let photo = (Some("PHOTO"), Some(10));
        let cases = [
            ("pro", RuleAction::Copy, photo, MediaRoute::Relay),
            ("pro", RuleAction::Resend, photo, MediaRoute::Relay),
            ("pro", RuleAction::Forward, photo, MediaRoute::Direct),
            ("free", RuleAction::Copy, photo, MediaRoute::TextOnly),
            ("free", RuleAction::Forward, photo, MediaRoute::Direct),
            (
                "pro",
                RuleAction::Copy,
                (Some("TEXT"), None),
                MediaRoute::Direct,
            ),
            ("free", RuleAction::Copy, (None, None), MediaRoute::Direct),
            (
                "pro",
                RuleAction::Copy,
                (Some("VIDEO"), Some(LIMIT + 1)),
                MediaRoute::TooLarge,
            ),
            (
                "pro",
                RuleAction::Copy,
                (Some("VIDEO"), Some(LIMIT)),
                MediaRoute::Relay,
            ),
            (
                "pro",
                RuleAction::Copy,
                (Some("PHOTO"), None),
                MediaRoute::Relay,
            ),
            (
                "free",
                RuleAction::Copy,
                (Some("VIDEO"), Some(LIMIT + 1)),
                MediaRoute::TextOnly,
            ),
        ];
        for (plan, action, (media_type, file_size), expected) in cases {
            assert_eq!(
                media_route(plan, action, media_type, file_size),
                expected,
                "{plan} {action:?} {media_type:?} {file_size:?}"
            );
        }
    }
}
