//! G4 生产历史指纹重算：隔离裁剪副本上的裁剪、只读扫描与销毁。
//! 定义处是 `docs/plan/proposals/G4-production-fingerprint-readonly-access.md`
//! §二、§三、§五 与 `docs/plan/evidence/G4生产历史指纹重算范围授权模板-2026-11-23.md`。
//!
//! 本模块不向生产数据库添加任何角色、GRANT、函数、视图或迁移（提案 §四）。
//! 三个动作各自只作用于责任方单独批准的一次性隔离副本：
//!
//! - [`run_trim`]：从已批准恢复副本按范围读取最小列集合，写入空的裁剪副本，
//!   并写入范围标记表 [`SCOPE_MARKER_TABLE`]。源侧读取纪律由备份读取授权约束；
//!   代码侧只保证源身份通过 [`verify_scan_identity`]、源事务只读且按租户
//!   设置隔离上下文。
//! - [`run_scan`]：以临时只读扫描身份在裁剪副本上重算指纹，只输出聚合计数。
//!   执行前先过 [`verify_scan_identity`]，且目标必须带范围标记 ——
//!   生产库天然没有这张表，这是 fail-closed 开关。
//! - [`run_destroy`]：删除裁剪副本里的标记表与四张裁剪表；同样以标记存在为前提。
//!
//! 输出纪律：扫描只在隔离进程内存中重算指纹，报告只含聚合计数与停止标志；
//! 消息正文、关键词、sender id、存储指纹值与 `content_hash` 不进入任何
//! 返回值、日志或错误信息（提案 §二 第 5 条）。

use sqlx::types::chrono::{DateTime, Utc};
use sqlx::{AssertSqlSafe, PgPool, Row};
use std::time::{Duration, Instant};
use tgm_core::notification::notification_fingerprint;

/// 范围标记表。只由裁剪器随裁剪副本创建；扫描与销毁以它的存在作为前置条件。
pub const SCOPE_MARKER_TABLE: &str = "g4_scan_scope";

/// 单批读取上限。隔离副本上没有并发业务写入，批大小只影响往返次数。
const MAX_BATCH: i64 = 500;

/// 扫描执行身份不得是、也不得切换到这些角色（提案 §二 第 4 条）。
/// 名单覆盖生产库的全部登录与组角色；临时扫描身份只能是隔离副本里
/// 另行创建的最小权限角色。名单只管「是不是生产角色」，不替代
/// [`crate::preflight::check_runtime_role`] 的特权属性检查。
pub const FORBIDDEN_ROLES: &[&str] = &[
    "postgres",
    "tgm_owner",
    "app_user",
    "auth_lookup",
    "platform_ops",
    "auth_cleanup",
    "tenant_cleanup",
    "shared_cleanup",
    "media_cleanup",
    "platform_cleanup",
    "tenant_lifecycle",
    "tenant_purge",
    "tenant_purge_batcher",
    "tenant_purge_finalizer",
];

/// 扫描失败。每一条都让整次动作不产出结果 —— 聚合计数不允许部分可信。
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("缺少范围标记表 {SCOPE_MARKER_TABLE}：本命令只允许在获批裁剪副本上运行")]
    MarkerMissing,
    #[error("范围标记与批准记录不一致：{0}")]
    MarkerMismatch(String),
    #[error("批准范围未允许在隔离执行内存中读取内容字段，拒绝重算")]
    ContentNotApproved,
    #[error("扫描执行身份不满足最小权限要求：{0}")]
    Identity(String),
    #[error("扫描执行身份是或可切换到被禁角色")]
    ForbiddenRole,
    #[error("裁剪目标库不满足一次性副本前提：{0}")]
    Destination(String),
    #[error("范围参数无效：{0}")]
    InvalidScope(String),
    #[error(transparent)]
    Query(#[from] sqlx::Error),
}

/// 已批准的扫描范围。字段与责任方签字申请逐项绑定（提案 §三），
/// 由 [`run_trim`] 写入裁剪副本的标记表，由 [`load_marker`] 读回。
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovedScope {
    pub approval_ref: String,
    pub tenants: Vec<i64>,
    pub window_start: Option<DateTime<Utc>>,
    pub window_end: Option<DateTime<Utc>>,
    pub as_of: DateTime<Utc>,
    pub include_expired: bool,
    pub allow_content_read: bool,
    pub max_rows: i64,
    pub max_duration_secs: i64,
}

/// 扫描报告。只有聚合计数与停止标志 —— 见模块头部的输出纪律。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScanReport {
    pub tenants_scanned: i64,
    pub scanned: i64,
    pub matched: i64,
    pub mismatched: i64,
    pub not_recomputable_no_task: i64,
    pub not_recomputable_missing_source: i64,
    pub not_recomputable_scrubbed: i64,
    pub stopped_by_rows: bool,
    pub stopped_by_duration: bool,
}

/// 裁剪结果。同样只含计数。`stopped_by_rows` 为真时标记未写入，
/// 整份目标库必须整体废弃（DROP DATABASE），不得继续裁剪或扫描。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrimReport {
    pub notify_dedup_rows: i64,
    pub delivery_task_rows: i64,
    pub tenant_event_rows: i64,
    pub canonical_event_rows: i64,
    pub stopped_by_rows: bool,
}

/// 解析 RFC 3339 时间。给 CLI 用：bins 侧不直接依赖 chrono。
pub fn parse_rfc3339(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|error| format!("{value} 不是有效的 RFC 3339 时间：{error}"))
}

/// 校验扫描执行身份（提案 §二 第 4 条）：
/// 先过启动期角色自检（非超级用户、无 BYPASSRLS、不持有特权组成员、
/// 不是 public 表 owner），再确认它不是也无法切换到任何生产角色。
/// `pg_has_role(..., 'USAGE')` 含间接成员与角色自身。
pub async fn verify_scan_identity(pool: &PgPool) -> Result<String, ScanError> {
    let role = crate::preflight::check_runtime_role(pool)
        .await
        .map_err(|error| ScanError::Identity(error.to_string()))?;
    let forbidden: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_roles r
             WHERE r.rolname = ANY($1)
               AND pg_has_role(current_user, r.oid, 'USAGE')
         )",
    )
    .bind(FORBIDDEN_ROLES)
    .fetch_one(pool)
    .await?;
    if forbidden {
        return Err(ScanError::ForbiddenRole);
    }
    Ok(role)
}

/// 裁剪源（已批准恢复副本）的身份检查。与 [`verify_scan_identity`] 的差别：
/// 不查「是否 public 表 owner」—— 恢复副本的表就是由隔离管理员建立/恢复的，
/// 拥有者身份是正常形态。其余防线不变：拒绝超级用户、BYPASSRLS、
/// 特权组成员与 [`FORBIDDEN_ROLES`] 里的全部生产角色 —— 生产库上不存在
/// 能通过本检查的既有身份，直接指向生产库即失败。
pub async fn verify_trim_source_identity(pool: &PgPool) -> Result<String, ScanError> {
    let row = sqlx::query(
        "SELECT current_user::text AS role, rolsuper, rolbypassrls
           FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(pool)
    .await?;
    let role: String = row.get("role");
    for (flag, attr) in [("rolsuper", "SUPERUSER"), ("rolbypassrls", "BYPASSRLS")] {
        if row.get::<bool, _>(flag) {
            return Err(ScanError::Identity(format!("{role} 带 {attr}")));
        }
    }
    let escalation: Option<String> = sqlx::query_scalar(
        "SELECT r.rolname::text
           FROM pg_roles r
          WHERE pg_has_role(current_user, r.oid, 'USAGE')
            AND r.rolname <> current_user
            AND (r.rolsuper OR r.rolbypassrls)
          ORDER BY 1 LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    if let Some(grantor) = escalation {
        return Err(ScanError::Identity(format!(
            "{role} 是 {grantor} 的成员，后者带 SUPERUSER 或 BYPASSRLS"
        )));
    }
    let forbidden: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_roles r
             WHERE r.rolname = ANY($1)
               AND pg_has_role(current_user, r.oid, 'USAGE')
         )",
    )
    .bind(FORBIDDEN_ROLES)
    .fetch_one(pool)
    .await?;
    if forbidden {
        return Err(ScanError::ForbiddenRole);
    }
    Ok(role)
}

async fn marker_exists(pool: &PgPool) -> Result<bool, ScanError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_class c
            JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'public' AND c.relname = $1 AND c.relkind = 'r'
         )",
    )
    .bind(SCOPE_MARKER_TABLE)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// 读取并核对范围标记。`expected_approval_ref` 由批准记录提供；
/// 标记表不存在或查不到对应行即拒绝运行。
pub async fn load_marker(
    pool: &PgPool,
    expected_approval_ref: &str,
) -> Result<ApprovedScope, ScanError> {
    if !marker_exists(pool).await? {
        return Err(ScanError::MarkerMissing);
    }
    let row = sqlx::query(
        "SELECT approval_ref, tenants, window_start, window_end, as_of,
                include_expired, allow_content_read, max_rows, max_duration_secs
           FROM g4_scan_scope
          WHERE approval_ref = $1",
    )
    .bind(expected_approval_ref)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Err(ScanError::MarkerMismatch(format!(
            "标记表中不存在批准记录 {expected_approval_ref}"
        )));
    };
    let scope = ApprovedScope {
        approval_ref: row.get("approval_ref"),
        tenants: row.get("tenants"),
        window_start: row.get("window_start"),
        window_end: row.get("window_end"),
        as_of: row.get("as_of"),
        include_expired: row.get("include_expired"),
        allow_content_read: row.get("allow_content_read"),
        max_rows: row.get("max_rows"),
        max_duration_secs: row.get("max_duration_secs"),
    };
    if scope.tenants.is_empty() {
        return Err(ScanError::MarkerMismatch("租户范围为空".to_owned()));
    }
    if scope.max_rows <= 0 || scope.max_duration_secs <= 0 {
        return Err(ScanError::MarkerMismatch(
            "行数上限与时长上限必须为正".to_owned(),
        ));
    }
    Ok(scope)
}

/// 开始一个只读事务并完成租户上下文设置。
///
/// 不复用 [`crate::tenant::tenant_tx`]：那里的第一参数是 `TenantId`，
/// 而该 newtype 刻意不提供从整数构造的路径（eng/00 §4.1）—— 业务代码的
/// 租户身份必须来自鉴权上下文。本模块的租户编号来自获批范围标记，
/// 是维护工具而非业务路径，故直接用 `set_config` 绑参，形态与
/// `tenant_tx` 相同：先 `begin` 再 `set_config`（事务外调用会静默失效）。
async fn begin_readonly_tenant_tx(
    pool: &PgPool,
    tenant: i64,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// 一条 `SENT` 记录在扫描查询里的完整形态。关联全部用 LEFT JOIN：
/// 缺失的环节各自对应一种 `not_recomputable` 原因，不得被静默吞掉。
struct ScanRow {
    stored_fingerprint: String,
    cursor_target: i64,
    has_task: bool,
    matched_keywords: Option<Vec<String>>,
    sender_id: Option<i64>,
    text: Option<String>,
    caption: Option<String>,
}

/// 只读扫描。在标记表批准的租户与时间范围内，对每条 `SENT` 去重记录
/// 沿 `notify_dedup -> delivery_tasks -> tenant_events -> canonical_events`
/// 关联，用 [`notification_fingerprint`] 在内存中重算并只累计计数。
///
/// 事务形态：每个租户一个只读事务；裁剪表上的租户隔离策略与业务表同形。
/// 行数与时长取「请求值与标记值的较小者」—— 批准范围只能收紧不能放宽。
/// 命中任一上限即停止，停止标志进入报告（提案 §五 第 1 条）。
pub async fn run_scan(
    pool: &PgPool,
    scope: &ApprovedScope,
    requested_duration: Duration,
    max_rows_override: Option<i64>,
) -> Result<ScanReport, ScanError> {
    if !scope.allow_content_read {
        return Err(ScanError::ContentNotApproved);
    }
    verify_scan_identity(pool).await?;
    let max_rows = match max_rows_override {
        Some(requested) if requested > 0 => requested.min(scope.max_rows),
        Some(_) => {
            return Err(ScanError::InvalidScope("行数上限必须为正".to_owned()));
        }
        None => scope.max_rows,
    };
    let marker_duration =
        Duration::from_secs(u64::try_from(scope.max_duration_secs).unwrap_or(u64::MAX));
    let max_duration = requested_duration.min(marker_duration);
    let started = Instant::now();
    let mut report = ScanReport::default();
    for tenant in &scope.tenants {
        if report.scanned >= max_rows || started.elapsed() >= max_duration {
            break;
        }
        report.tenants_scanned += 1;
        scan_tenant(
            pool,
            *tenant,
            scope,
            max_rows,
            max_duration,
            started,
            &mut report,
        )
        .await?;
    }
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn scan_tenant(
    pool: &PgPool,
    tenant: i64,
    scope: &ApprovedScope,
    max_rows: i64,
    max_duration: Duration,
    started: Instant,
    report: &mut ScanReport,
) -> Result<(), ScanError> {
    let mut cursor_fingerprint = String::new();
    let mut cursor_target: i64 = 0;
    loop {
        if report.scanned >= max_rows {
            report.stopped_by_rows = true;
            return Ok(());
        }
        if started.elapsed() >= max_duration {
            report.stopped_by_duration = true;
            return Ok(());
        }
        let remaining = max_rows - report.scanned;
        let batch_limit = MAX_BATCH.min(remaining);
        // 静态 SQL：批准窗口过滤（$1..$4）内联于此，与裁剪侧语义相同、
        // 行别名不同。动态拼 SQL 会撞上 sqlx 的注入审计，过滤文本也
        // 本来就不该在运行期变化。
        const QUERY: &str = "SELECT dedup.fingerprint AS stored_fingerprint,
                    dedup.target_ref AS cursor_target,
                    task.id IS NOT NULL AS has_task,
                    tenant_event.matched_keywords,
                    event.sender_id,
                    event.text,
                    event.caption
               FROM notify_dedup AS dedup
               LEFT JOIN LATERAL (
                     SELECT id, tenant_event_id
                       FROM delivery_tasks
                      WHERE tenant_id = dedup.tenant_id
                        AND notification_fingerprint = dedup.fingerprint
                        AND notification_target_ref = dedup.target_ref
                        AND status = 'DONE'
                        AND completed_at <= dedup.last_sent_at
                      ORDER BY completed_at DESC, id DESC
                      LIMIT 1
                   ) AS task ON TRUE
               LEFT JOIN tenant_events AS tenant_event
                 ON tenant_event.tenant_id = dedup.tenant_id
                AND tenant_event.id = task.tenant_event_id
               LEFT JOIN canonical_events AS event
                 ON event.id = tenant_event.canonical_event_id
              WHERE dedup.status = 'SENT'
                AND dedup.last_sent_at <= $1
                AND ($2::boolean OR dedup.expires_at > $1)
                AND ($3::timestamptz IS NULL OR dedup.last_sent_at >= $3)
                AND ($4::timestamptz IS NULL OR dedup.last_sent_at < $4)
                AND (dedup.fingerprint, dedup.target_ref) > ($5, $6)
              ORDER BY dedup.fingerprint, dedup.target_ref
              LIMIT $7";
        let mut tx = begin_readonly_tenant_tx(pool, tenant).await?;
        let fetched = sqlx::query(QUERY)
            .bind(scope.as_of)
            .bind(scope.include_expired)
            .bind(scope.window_start)
            .bind(scope.window_end)
            .bind(&cursor_fingerprint)
            .bind(cursor_target)
            .bind(batch_limit)
            .fetch_all(&mut *tx)
            .await?;
        tx.rollback().await?;
        let batch_len = fetched.len();
        if batch_len == 0 {
            return Ok(());
        }
        for row in fetched {
            let scan_row = ScanRow {
                stored_fingerprint: row.get("stored_fingerprint"),
                cursor_target: row.get("cursor_target"),
                has_task: row.get("has_task"),
                matched_keywords: row.get("matched_keywords"),
                sender_id: row.get("sender_id"),
                text: row.get("text"),
                caption: row.get("caption"),
            };
            cursor_fingerprint.clone_from(&scan_row.stored_fingerprint);
            cursor_target = scan_row.cursor_target;
            classify_one(scan_row, report);
            report.scanned += 1;
            if report.scanned >= max_rows || started.elapsed() >= max_duration {
                break;
            }
        }
        if report.scanned >= max_rows {
            report.stopped_by_rows = true;
            return Ok(());
        }
        if started.elapsed() >= max_duration {
            report.stopped_by_duration = true;
            return Ok(());
        }
        if batch_len < batch_limit as usize {
            return Ok(());
        }
    }
}

/// 单条分类。只改计数器 —— 任何字段值都不得离开本函数。
fn classify_one(row: ScanRow, report: &mut ScanReport) {
    if !row.has_task {
        report.not_recomputable_no_task += 1;
        return;
    }
    let Some(matched_keywords) = row.matched_keywords else {
        // 命中词缺失意味着源字段不完整（未处理完或已脱敏），不猜测原值。
        report.not_recomputable_missing_source += 1;
        return;
    };
    let text_empty = row.text.as_deref().is_none_or(str::is_empty);
    let caption_empty = row.caption.as_deref().is_none_or(str::is_empty);
    if row.sender_id.is_none() && text_empty && caption_empty {
        // 全部指纹输入被清空：按 G4-L3 既定隐私边界记为不可复算样本，
        // 不声称仍可重算（docs/plan/proposals/G4-L3-notify-dedup.md）。
        report.not_recomputable_scrubbed += 1;
        return;
    }
    let recomputed = notification_fingerprint(
        row.sender_id,
        &matched_keywords,
        row.text.as_deref(),
        row.caption.as_deref(),
    );
    if recomputed == row.stored_fingerprint {
        report.matched += 1;
    } else {
        report.mismatched += 1;
    }
}

/// 校验裁剪范围参数本身（与标记核对是 [`load_marker`] 的事）。
pub fn validate_trim_scope(scope: &ApprovedScope, scan_role: &str) -> Result<(), ScanError> {
    let approval_ref = &scope.approval_ref;
    if approval_ref.is_empty() || approval_ref.len() > 200 {
        return Err(ScanError::InvalidScope(
            "批准记录编号长度必须在 1-200".to_owned(),
        ));
    }
    if !approval_ref.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(ScanError::InvalidScope(
            "批准记录编号含不可打印字符".to_owned(),
        ));
    }
    if scope.tenants.is_empty() {
        return Err(ScanError::InvalidScope("租户范围不能为空".to_owned()));
    }
    let mut sorted = scope.tenants.clone();
    sorted.sort_unstable();
    if sorted.iter().any(|id| *id <= 0) || sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(ScanError::InvalidScope(
            "租户编号必须为正且不得重复".to_owned(),
        ));
    }
    if let (Some(start), Some(end)) = (scope.window_start, scope.window_end)
        && start >= end
    {
        return Err(ScanError::InvalidScope(
            "时间窗口起点必须早于终点".to_owned(),
        ));
    }
    if scope.max_rows <= 0 || scope.max_duration_secs <= 0 {
        return Err(ScanError::InvalidScope(
            "行数上限与时长上限必须为正".to_owned(),
        ));
    }
    validate_scan_role_name(scan_role)?;
    Ok(())
}

fn validate_scan_role_name(role: &str) -> Result<(), ScanError> {
    let valid = role.len() <= 63
        && role
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
        && role
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
    if !valid {
        return Err(ScanError::InvalidScope(
            "扫描角色名必须是小写字母开头的 [a-z0-9_]，且不超 63 字符".to_owned(),
        ));
    }
    if FORBIDDEN_ROLES.contains(&role) {
        return Err(ScanError::ForbiddenRole);
    }
    Ok(())
}

const MARKER_DDL: &str = "CREATE TABLE IF NOT EXISTS g4_scan_scope (
    approval_ref TEXT PRIMARY KEY,
    tenants BIGINT[] NOT NULL,
    window_start TIMESTAMPTZ,
    window_end TIMESTAMPTZ,
    as_of TIMESTAMPTZ NOT NULL,
    include_expired BOOLEAN NOT NULL,
    allow_content_read BOOLEAN NOT NULL,
    max_rows BIGINT NOT NULL CHECK (max_rows > 0),
    max_duration_secs BIGINT NOT NULL CHECK (max_duration_secs > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
)";

const TRIM_TABLE_DDLS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS notify_dedup (
        tenant_id BIGINT NOT NULL,
        fingerprint TEXT NOT NULL,
        target_ref BIGINT NOT NULL,
        status TEXT NOT NULL,
        last_sent_at TIMESTAMPTZ NOT NULL,
        expires_at TIMESTAMPTZ NOT NULL,
        PRIMARY KEY (tenant_id, fingerprint, target_ref)
    )",
    "CREATE TABLE IF NOT EXISTS delivery_tasks (
        tenant_id BIGINT NOT NULL,
        id BIGINT NOT NULL,
        tenant_event_id BIGINT,
        notification_fingerprint TEXT,
        notification_target_ref BIGINT,
        status TEXT NOT NULL,
        completed_at TIMESTAMPTZ,
        UNIQUE (tenant_id, id)
    )",
    "CREATE TABLE IF NOT EXISTS tenant_events (
        tenant_id BIGINT NOT NULL,
        id BIGINT NOT NULL,
        canonical_event_id BIGINT NOT NULL,
        matched_keywords TEXT[],
        UNIQUE (tenant_id, id)
    )",
    "CREATE TABLE IF NOT EXISTS canonical_events (
        id BIGINT PRIMARY KEY,
        sender_id BIGINT,
        text TEXT,
        caption TEXT,
        content_hash TEXT
    )",
];

/// 裁剪（提案 §二 第 3 条）：从源库（已批准的恢复副本）按范围读取四张表的
/// 最小列集合，写入目标裁剪副本，并写入范围标记。
///
/// 前置条件：源连接通过 [`verify_trim_source_identity`]（拒绝超级用户、
/// BYPASSRLS 与全部生产角色 —— 直接指向生产库时找不到可用身份）；
/// 目标库必须没有标记表且四张裁剪表无数据，保证副本一次性。扫描角色名只用于目标库上的 GRANT 与策略，角色本身由责任方
/// 在隔离环境另行创建（本模块不创建生产角色）。
///
/// 源侧每个租户一个只读事务并设置 `app.tenant_id` —— 恢复副本沿用生产
/// RLS 形态，隔离管理员按裁剪角色配置同形策略后方可读取。
pub async fn run_trim(
    source: &PgPool,
    dest: &PgPool,
    scope: &ApprovedScope,
    scan_role: &str,
) -> Result<TrimReport, ScanError> {
    validate_trim_scope(scope, scan_role)?;
    verify_trim_source_identity(source).await?;
    if marker_exists(dest).await? {
        return Err(ScanError::Destination(
            "目标库已存在范围标记；裁剪副本不得重复使用，先销毁".to_owned(),
        ));
    }
    // 表名全部来自字面量，逐条静态查询 —— 不用动态拼名。
    // 分两段：先问表在不在，再问有没有行。合成一条时表不存在会让
    // EXISTS 子查询直接报 "relation does not exist"，而「表不存在」
    // 对空库检查来说是合法状态。
    const EXISTENCE_CHECKS: &[(&str, &str)] = &[
        (
            "notify_dedup",
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'notify_dedup')",
        ),
        (
            "delivery_tasks",
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'delivery_tasks')",
        ),
        (
            "tenant_events",
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'tenant_events')",
        ),
        (
            "canonical_events",
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'canonical_events')",
        ),
    ];
    const ROW_CHECKS: &[(&str, &str)] = &[
        ("notify_dedup", "SELECT EXISTS (SELECT 1 FROM notify_dedup)"),
        (
            "delivery_tasks",
            "SELECT EXISTS (SELECT 1 FROM delivery_tasks)",
        ),
        (
            "tenant_events",
            "SELECT EXISTS (SELECT 1 FROM tenant_events)",
        ),
        (
            "canonical_events",
            "SELECT EXISTS (SELECT 1 FROM canonical_events)",
        ),
    ];
    for ((table, existence), (_, row_check)) in EXISTENCE_CHECKS.iter().zip(ROW_CHECKS) {
        let exists: bool = sqlx::query_scalar(*existence).fetch_one(dest).await?;
        if !exists {
            continue;
        }
        let has_rows: bool = sqlx::query_scalar(*row_check).fetch_one(dest).await?;
        if has_rows {
            return Err(ScanError::Destination(format!(
                "目标库 {table} 已有数据；裁剪目标必须是空库"
            )));
        }
    }

    let mut tx = dest.begin().await?;
    // 装载角色 = 目标连接自己的身份。它负责建表与灌数，之后由销毁步骤
    // 连同副本一起回收；扫描角色只拿 SELECT。
    let loader_role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *tx)
        .await?;
    validate_scan_role_name(&loader_role)
        .map_err(|error| ScanError::Destination(format!("目标连接角色名不合法：{error}")))?;
    sqlx::query(MARKER_DDL).execute(&mut *tx).await?;
    for ddl in TRIM_TABLE_DDLS {
        sqlx::query(*ddl).execute(&mut *tx).await?;
    }
    const RLS_DDL: &[&str] = &[
        "ALTER TABLE notify_dedup ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE notify_dedup FORCE ROW LEVEL SECURITY",
        "DROP POLICY IF EXISTS g4_scan_tenant_isolation ON notify_dedup",
        "DROP POLICY IF EXISTS g4_scan_loader ON notify_dedup",
        "ALTER TABLE delivery_tasks ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE delivery_tasks FORCE ROW LEVEL SECURITY",
        "DROP POLICY IF EXISTS g4_scan_tenant_isolation ON delivery_tasks",
        "DROP POLICY IF EXISTS g4_scan_loader ON delivery_tasks",
        "ALTER TABLE tenant_events ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE tenant_events FORCE ROW LEVEL SECURITY",
        "DROP POLICY IF EXISTS g4_scan_tenant_isolation ON tenant_events",
        "DROP POLICY IF EXISTS g4_scan_loader ON tenant_events",
        "ALTER TABLE canonical_events ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE canonical_events FORCE ROW LEVEL SECURITY",
        "DROP POLICY IF EXISTS g4_scan_loader ON canonical_events",
        "DROP POLICY IF EXISTS g4_scan_canonical_read ON canonical_events",
        "DROP POLICY IF EXISTS g4_scan_loader ON g4_scan_scope",
        "DROP POLICY IF EXISTS g4_scan_marker_read ON g4_scan_scope",
    ];
    for ddl in RLS_DDL {
        sqlx::query(*ddl).execute(&mut *tx).await?;
    }
    sqlx::query("ALTER TABLE canonical_events ENABLE ROW LEVEL SECURITY")
        .execute(&mut *tx)
        .await?;
    sqlx::query("ALTER TABLE g4_scan_scope ENABLE ROW LEVEL SECURITY")
        .execute(&mut *tx)
        .await?;
    // 动态拼接仅限角色名。`scan_role` 经 `validate_trim_scope` 限定在
    // `^[a-z][a-z0-9_]*$` 且不在生产角色名单里；`loader_role` 取自目录并
    // 过同一个校验（注入面已审计），故用 AssertSqlSafe 显式放行 ——
    // 与隔离测试建探针角色的同形处理。
    for table in [
        "notify_dedup",
        "delivery_tasks",
        "tenant_events",
        "canonical_events",
    ] {
        let loader_policy = format!(
            "CREATE POLICY g4_scan_loader ON {table} FOR ALL TO {loader_role}
              USING (true) WITH CHECK (true)"
        );
        sqlx::query(AssertSqlSafe(loader_policy))
            .execute(&mut *tx)
            .await?;
    }
    let marker_loader_policy = format!(
        "CREATE POLICY g4_scan_loader ON g4_scan_scope FOR ALL TO {loader_role}
          USING (true) WITH CHECK (true)"
    );
    sqlx::query(AssertSqlSafe(marker_loader_policy))
        .execute(&mut *tx)
        .await?;
    for table in ["notify_dedup", "delivery_tasks", "tenant_events"] {
        let policy = format!(
            "CREATE POLICY g4_scan_tenant_isolation ON {table} FOR SELECT TO {scan_role}
              USING (tenant_id = current_setting('app.tenant_id')::bigint)"
        );
        sqlx::query(AssertSqlSafe(policy)).execute(&mut *tx).await?;
    }
    // canonical_events 与标记表没有租户列：副本内容本身就是批准范围，
    // 扫描角色按整表可读处理；写入不开放。
    let canonical_policy = format!(
        "CREATE POLICY g4_scan_canonical_read ON canonical_events FOR SELECT TO {scan_role}
          USING (true)"
    );
    sqlx::query(AssertSqlSafe(canonical_policy))
        .execute(&mut *tx)
        .await?;
    let marker_policy = format!(
        "CREATE POLICY g4_scan_marker_read ON g4_scan_scope FOR SELECT TO {scan_role}
          USING (true)"
    );
    sqlx::query(AssertSqlSafe(marker_policy))
        .execute(&mut *tx)
        .await?;
    sqlx::query(AssertSqlSafe(format!(
        "GRANT USAGE ON SCHEMA public TO {scan_role}"
    )))
    .execute(&mut *tx)
    .await?;
    for table in [
        "notify_dedup",
        "delivery_tasks",
        "tenant_events",
        "canonical_events",
        SCOPE_MARKER_TABLE,
    ] {
        sqlx::query(AssertSqlSafe(format!(
            "GRANT SELECT ON {table} TO {scan_role}"
        )))
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    let mut report = TrimReport::default();
    for tenant in &scope.tenants {
        if report.stopped_by_rows {
            break;
        }
        trim_tenant(source, dest, *tenant, scope, &mut report).await?;
    }

    if report.stopped_by_rows {
        // 命中行数上限说明副本不完整；不写标记，整份目标库必须整体废弃。
        return Ok(report);
    }
    sqlx::query(
        "INSERT INTO g4_scan_scope
            (approval_ref, tenants, window_start, window_end, as_of,
             include_expired, allow_content_read, max_rows, max_duration_secs)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(&scope.approval_ref)
    .bind(&scope.tenants)
    .bind(scope.window_start)
    .bind(scope.window_end)
    .bind(scope.as_of)
    .bind(scope.include_expired)
    .bind(scope.allow_content_read)
    .bind(scope.max_rows)
    .bind(scope.max_duration_secs)
    .execute(dest)
    .await?;
    Ok(report)
}

/// 选中的投递任务形态。裁剪只保留重算所需的最小列。
#[derive(Debug)]
struct ChosenTask {
    id: i64,
    tenant_event_id: Option<i64>,
    fingerprint: Option<String>,
    target_ref: Option<i64>,
    completed_at: Option<DateTime<Utc>>,
}

/// 任务候选的排序中间形态。
#[derive(Debug)]
struct TaskCandidate {
    completed_at: Option<DateTime<Utc>>,
    id: i64,
    tenant_event_id: Option<i64>,
    fingerprint: Option<String>,
    target_ref: Option<i64>,
}

/// 单租户裁剪。源侧只读事务按租户设置隔离上下文，按
/// (fingerprint, target_ref) 游标分批；每批在目标库一个事务里落四张表，
/// `ON CONFLICT DO NOTHING` 兜跨批重复。
async fn trim_tenant(
    source: &PgPool,
    dest: &PgPool,
    tenant: i64,
    scope: &ApprovedScope,
    report: &mut TrimReport,
) -> Result<(), ScanError> {
    let mut cursor_fingerprint = String::new();
    let mut cursor_target: i64 = 0;
    loop {
        if report.notify_dedup_rows >= scope.max_rows {
            report.stopped_by_rows = true;
            return Ok(());
        }
        let remaining = scope.max_rows - report.notify_dedup_rows;
        let batch_limit = MAX_BATCH.min(remaining);

        let mut source_tx = begin_readonly_tenant_tx(source, tenant).await?;
        const DEDUP_QUERY: &str = "SELECT fingerprint, target_ref, status, last_sent_at, expires_at
               FROM notify_dedup
              WHERE tenant_id = $5
                AND status = 'SENT'
                AND last_sent_at <= $1
                AND ($2::boolean OR expires_at > $1)
                AND ($3::timestamptz IS NULL OR last_sent_at >= $3)
                AND ($4::timestamptz IS NULL OR last_sent_at < $4)
                AND (fingerprint, target_ref) > ($6, $7)
              ORDER BY fingerprint, target_ref
              LIMIT $8";
        let dedup_rows = sqlx::query(DEDUP_QUERY)
            .bind(scope.as_of)
            .bind(scope.include_expired)
            .bind(scope.window_start)
            .bind(scope.window_end)
            .bind(tenant)
            .bind(&cursor_fingerprint)
            .bind(cursor_target)
            .bind(batch_limit)
            .fetch_all(&mut *source_tx)
            .await?;
        if dedup_rows.is_empty() {
            return Ok(());
        }
        let batch_len = dedup_rows.len();

        let mut fingerprints: Vec<String> = Vec::with_capacity(batch_len);
        for row in &dedup_rows {
            fingerprints.push(row.get("fingerprint"));
        }
        let tasks = sqlx::query(
            "SELECT id, tenant_event_id, notification_fingerprint,
                    notification_target_ref, completed_at
               FROM delivery_tasks
              WHERE tenant_id = $1 AND status = 'DONE'
                AND notification_fingerprint = ANY($2)",
        )
        .bind(tenant)
        .bind(&fingerprints)
        .fetch_all(&mut *source_tx)
        .await?;

        // 为每条去重记录选一条任务：fingerprint/target 相等、
        // completed_at <= last_sent_at，按 (completed_at, id) 取最大。
        // 与扫描查询里的 LATERAL 选择逻辑一致。
        let mut chosen: Vec<ChosenTask> = Vec::new();
        for dedup in &dedup_rows {
            let fingerprint: &str = dedup.get("fingerprint");
            let target_ref: i64 = dedup.get("target_ref");
            let last_sent_at: DateTime<Utc> = dedup.get("last_sent_at");
            let mut best: Option<TaskCandidate> = None;
            for task in &tasks {
                let task_fp: Option<String> = task.get("notification_fingerprint");
                let task_target: Option<i64> = task.get("notification_target_ref");
                let completed_at: Option<DateTime<Utc>> = task.get("completed_at");
                if task_fp.as_deref() != Some(fingerprint)
                    || task_target != Some(target_ref)
                    || completed_at.is_none_or(|completed| completed > last_sent_at)
                {
                    continue;
                }
                let candidate = TaskCandidate {
                    completed_at,
                    id: task.get("id"),
                    tenant_event_id: task.get("tenant_event_id"),
                    fingerprint: task_fp,
                    target_ref: task_target,
                };
                if best.as_ref().is_none_or(|current| {
                    (candidate.completed_at, candidate.id) > (current.completed_at, current.id)
                }) {
                    best = Some(candidate);
                }
            }
            if let Some(candidate) = best {
                chosen.push(ChosenTask {
                    id: candidate.id,
                    tenant_event_id: candidate.tenant_event_id,
                    fingerprint: candidate.fingerprint,
                    target_ref: candidate.target_ref,
                    completed_at: candidate.completed_at,
                });
            }
        }

        let mut tenant_event_ids: Vec<i64> = chosen
            .iter()
            .filter_map(|task| task.tenant_event_id)
            .collect();
        tenant_event_ids.sort_unstable();
        tenant_event_ids.dedup();
        let tenant_events = if tenant_event_ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query(
                "SELECT id, canonical_event_id, matched_keywords
                   FROM tenant_events
                  WHERE tenant_id = $1 AND id = ANY($2)",
            )
            .bind(tenant)
            .bind(&tenant_event_ids)
            .fetch_all(&mut *source_tx)
            .await?
        };
        let mut canonical_ids: Vec<i64> = tenant_events
            .iter()
            .map(|event| event.get("canonical_event_id"))
            .collect();
        canonical_ids.sort_unstable();
        canonical_ids.dedup();
        let canonical_events = if canonical_ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query(
                "SELECT id, sender_id, text, caption, content_hash
                   FROM canonical_events
                  WHERE id = ANY($1)",
            )
            .bind(&canonical_ids)
            .fetch_all(&mut *source_tx)
            .await?
        };
        source_tx.rollback().await?;

        let mut dest_tx = dest.begin().await?;
        // ponytail: 逐行 INSERT，十万行级扫描时改 COPY 或批量 VALUES
        for row in &dedup_rows {
            sqlx::query(
                "INSERT INTO notify_dedup
                    (tenant_id, fingerprint, target_ref, status, last_sent_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT DO NOTHING",
            )
            .bind(tenant)
            .bind(row.get::<String, _>("fingerprint"))
            .bind(row.get::<i64, _>("target_ref"))
            .bind(row.get::<String, _>("status"))
            .bind(row.get::<DateTime<Utc>, _>("last_sent_at"))
            .bind(row.get::<DateTime<Utc>, _>("expires_at"))
            .execute(&mut *dest_tx)
            .await?;
        }
        for task in &chosen {
            sqlx::query(
                "INSERT INTO delivery_tasks
                    (tenant_id, id, tenant_event_id, notification_fingerprint,
                     notification_target_ref, status, completed_at)
                 VALUES ($1, $2, $3, $4, $5, 'DONE', $6)
                 ON CONFLICT DO NOTHING",
            )
            .bind(tenant)
            .bind(task.id)
            .bind(task.tenant_event_id)
            .bind(&task.fingerprint)
            .bind(task.target_ref)
            .bind(task.completed_at)
            .execute(&mut *dest_tx)
            .await?;
        }
        for event in &tenant_events {
            sqlx::query(
                "INSERT INTO tenant_events (tenant_id, id, canonical_event_id, matched_keywords)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT DO NOTHING",
            )
            .bind(tenant)
            .bind(event.get::<i64, _>("id"))
            .bind(event.get::<i64, _>("canonical_event_id"))
            .bind(event.get::<Option<Vec<String>>, _>("matched_keywords"))
            .execute(&mut *dest_tx)
            .await?;
        }
        for event in &canonical_events {
            sqlx::query(
                "INSERT INTO canonical_events (id, sender_id, text, caption, content_hash)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT DO NOTHING",
            )
            .bind(event.get::<i64, _>("id"))
            .bind(event.get::<Option<i64>, _>("sender_id"))
            .bind(event.get::<Option<String>, _>("text"))
            .bind(event.get::<Option<String>, _>("caption"))
            .bind(event.get::<Option<String>, _>("content_hash"))
            .execute(&mut *dest_tx)
            .await?;
        }
        dest_tx.commit().await?;

        report.notify_dedup_rows += batch_len as i64;
        report.delivery_task_rows += chosen.len() as i64;
        report.tenant_event_rows += tenant_events.len() as i64;
        report.canonical_event_rows += canonical_events.len() as i64;

        let last = &dedup_rows[batch_len - 1];
        cursor_fingerprint = last.get("fingerprint");
        cursor_target = last.get("target_ref");
        if batch_len < batch_limit as usize {
            return Ok(());
        }
    }
}

/// 销毁（提案 §二 第 6 条）：删除裁剪副本里的标记表与四张裁剪表。
/// 前置条件与扫描相同 —— 没有标记表的数据库拒绝执行，避免把销毁
/// 指向任何非裁剪产物。角色与数据库本身的删除由责任方在报告后另行执行。
pub async fn run_destroy(pool: &PgPool) -> Result<Vec<String>, ScanError> {
    if !marker_exists(pool).await? {
        return Err(ScanError::MarkerMissing);
    }
    // 表名字面量逐条静态执行；销毁只认裁剪产物四表加标记表。
    const DROP_STATEMENTS: &[&str] = &[
        "DROP TABLE IF EXISTS notify_dedup CASCADE",
        "DROP TABLE IF EXISTS delivery_tasks CASCADE",
        "DROP TABLE IF EXISTS tenant_events CASCADE",
        "DROP TABLE IF EXISTS canonical_events CASCADE",
        "DROP TABLE IF EXISTS g4_scan_scope CASCADE",
    ];
    let mut dropped = Vec::new();
    for statement in DROP_STATEMENTS {
        sqlx::query(*statement).execute(pool).await?;
        let table = statement
            .trim_start_matches("DROP TABLE IF EXISTS ")
            .trim_end_matches(" CASCADE");
        dropped.push(table.to_owned());
    }
    Ok(dropped)
}

#[cfg(test)]
// 测试里的 expect 就是断言本身（同 repo.rs 测试形态）。
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn scope(approval_ref: &str, tenants: Vec<i64>) -> ApprovedScope {
        ApprovedScope {
            approval_ref: approval_ref.to_owned(),
            tenants,
            window_start: None,
            window_end: None,
            as_of: parse_rfc3339("2026-06-01T00:00:00Z").expect("解析失败"),
            include_expired: false,
            allow_content_read: true,
            max_rows: 100,
            max_duration_secs: 60,
        }
    }

    #[test]
    fn role_name_validation_rejects_injection_shapes() {
        assert!(validate_scan_role_name("g4_scan_ro").is_ok());
        for bad in [
            "",
            "G4_scan",
            "1scan",
            "scan-role",
            "scan; DROP",
            "scan' OR 1",
            &"a".repeat(64),
        ] {
            assert!(validate_scan_role_name(bad).is_err(), "应拒绝：{bad}");
        }
        for forbidden in FORBIDDEN_ROLES {
            assert!(
                matches!(
                    validate_scan_role_name(forbidden),
                    Err(ScanError::ForbiddenRole)
                ),
                "生产角色名必须被拒：{forbidden}"
            );
        }
    }

    #[test]
    fn trim_scope_validation_catches_bad_ranges() {
        assert!(validate_trim_scope(&scope("TEST-001", vec![1]), "g4_scan_ro").is_ok());
        assert!(validate_trim_scope(&scope("TEST-001", vec![]), "g4_scan_ro").is_err());
        assert!(validate_trim_scope(&scope("TEST-001", vec![1, 1]), "g4_scan_ro").is_err());
        assert!(validate_trim_scope(&scope("TEST-001", vec![0]), "g4_scan_ro").is_err());
        let mut bad = scope("TEST-001", vec![1]);
        bad.max_rows = 0;
        assert!(validate_trim_scope(&bad, "g4_scan_ro").is_err());
        let mut bad = scope("TEST-001", vec![1]);
        bad.max_duration_secs = 0;
        assert!(validate_trim_scope(&bad, "g4_scan_ro").is_err());
        let mut bad = scope("TEST-001", vec![1]);
        bad.approval_ref = String::new();
        assert!(validate_trim_scope(&bad, "g4_scan_ro").is_err());
        let mut bad = scope("TEST-001", vec![1]);
        bad.approval_ref = "x".repeat(201);
        assert!(validate_trim_scope(&bad, "g4_scan_ro").is_err());
        let mut bad = scope("TEST-001", vec![1]);
        bad.approval_ref = "a b".to_owned();
        assert!(validate_trim_scope(&bad, "g4_scan_ro").is_err());
        let mut bad = scope("TEST-001", vec![1]);
        bad.window_start = parse_rfc3339("2026-01-02T00:00:00Z").ok();
        bad.window_end = parse_rfc3339("2026-01-01T00:00:00Z").ok();
        assert!(validate_trim_scope(&bad, "g4_scan_ro").is_err());
        let mut good = scope("TEST-001", vec![1]);
        good.window_start = parse_rfc3339("2026-01-01T00:00:00Z").ok();
        good.window_end = parse_rfc3339("2026-01-02T00:00:00Z").ok();
        assert!(validate_trim_scope(&good, "g4_scan_ro").is_ok());
        assert!(validate_trim_scope(&scope("TEST-001", vec![1]), "app_user").is_err());
    }

    #[test]
    fn parse_rfc3339_rejects_garbage() {
        assert!(parse_rfc3339("2026-01-01T00:00:00Z").is_ok());
        assert!(parse_rfc3339("not-a-time").is_err());
    }

    #[test]
    fn classify_outputs_only_counts_and_respects_scrub() {
        let mut report = ScanReport::default();
        let stored = notification_fingerprint(Some(7), &["kw".to_owned()], Some("body"), None);
        classify_one(
            ScanRow {
                stored_fingerprint: stored.clone(),
                cursor_target: 1,
                has_task: true,
                matched_keywords: Some(vec!["kw".to_owned()]),
                sender_id: Some(7),
                text: Some("body".to_owned()),
                caption: None,
            },
            &mut report,
        );
        classify_one(
            ScanRow {
                stored_fingerprint: stored.clone(),
                cursor_target: 2,
                has_task: true,
                matched_keywords: Some(vec!["kw".to_owned()]),
                sender_id: Some(8),
                text: Some("other".to_owned()),
                caption: None,
            },
            &mut report,
        );
        classify_one(
            ScanRow {
                stored_fingerprint: stored.clone(),
                cursor_target: 3,
                has_task: false,
                matched_keywords: None,
                sender_id: None,
                text: None,
                caption: None,
            },
            &mut report,
        );
        classify_one(
            ScanRow {
                stored_fingerprint: stored.clone(),
                cursor_target: 4,
                has_task: true,
                matched_keywords: None,
                sender_id: None,
                text: None,
                caption: None,
            },
            &mut report,
        );
        classify_one(
            ScanRow {
                stored_fingerprint: stored,
                cursor_target: 5,
                has_task: true,
                matched_keywords: Some(vec!["kw".to_owned()]),
                sender_id: None,
                text: None,
                caption: None,
            },
            &mut report,
        );
        assert_eq!(report.matched, 1);
        assert_eq!(report.mismatched, 1);
        assert_eq!(report.not_recomputable_no_task, 1);
        assert_eq!(report.not_recomputable_missing_source, 1);
        assert_eq!(report.not_recomputable_scrubbed, 1);
    }
}
