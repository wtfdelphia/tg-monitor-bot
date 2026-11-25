//! G5 运行指标的隔离测试（提案 §四 判据）：
//!
//! - 三个核心指标全部从库取，受限角色 `app_user`，只读既有权限（不加 GRANT）；
//! - 水位线语义：重启后首个周期不回溯历史；
//! - 窗口与分母为零口径：DONE+DEAD 之外不计，CANCELLED 两边都不计；
//! - 租户消失 → 不报错、不出序列；采集失败不阻塞、健康位翻转；
//! - exposition 只含冻结清单的四个名称与 `tenant_id` 一个 label，
//!   内容数据不进 label。
#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use sqlx::PgPool;
use sqlx::types::chrono::Utc;
use tgm_app::metrics::{
    COLLECT_SUCCESS_METRIC, DELIVERY_DEAD_RATIO_METRIC, DELIVERY_QUEUE_DEPTH_METRIC,
    INGEST_LATENCY_METRIC, MetricsConfig, collect_sample, parse_listen_addr, record_sample, router,
};
use tower::ServiceExt;

async fn seed_task(
    admin: &support::AdminOnlyPool,
    tenant: tgm_core::ids::TenantId,
    sender: tgm_core::ids::IdentityId,
    status: &str,
    completed_at: &str,
) {
    sqlx::query(
        "INSERT INTO delivery_tasks
             (tenant_id, target_tg_chat_id, sender_identity_id, action, status, completed_at)
         VALUES ($1, -2001, $2, 'COPY', $3, $4)",
    )
    .bind(tenant)
    .bind(sender)
    .bind(status)
    .bind(
        sqlx::types::chrono::DateTime::parse_from_rfc3339(completed_at)
            .expect("测试时间必须合法")
            .with_timezone(&Utc),
    )
    .execute(admin.raw())
    .await
    .expect("写入投递任务失败");
}

/// 采集延迟只从 `canonical_events` 取（`created_at - message_date`），
/// 与租户层无关，故不播种 `tenant_events`。
async fn seed_event_with_age(
    admin: &support::AdminOnlyPool,
    identity: tgm_core::ids::IdentityId,
    message_id: i64,
    age_seconds: i64,
) {
    sqlx::query(
        "INSERT INTO canonical_events
             (adapter, receiver_identity_id, tg_chat_id, tg_message_id,
              event_type, message_date, created_at)
         VALUES ('bot', $1, -1001, $2, 'MESSAGE_CREATED',
                 clock_timestamp() - make_interval(secs => $3), clock_timestamp())",
    )
    .bind(identity)
    .bind(message_id)
    .bind(age_seconds)
    .execute(admin.raw())
    .await
    .expect("写入规范化事件失败");
}

fn config() -> MetricsConfig {
    MetricsConfig {
        listen_addr: "127.0.0.1:0".parse().expect("测试地址必须合法"),
        collect_interval: Duration::from_secs(30),
        failure_window: Duration::from_secs(3600),
        max_tenants: 1000,
        latency_sample_cap: 10_000,
    }
}

#[test]
fn listen_addr_must_be_loopback_and_well_formed() {
    assert!(parse_listen_addr("127.0.0.1:9090").is_ok());
    assert!(parse_listen_addr("[::1]:9090").is_ok());
    let wide = parse_listen_addr("0.0.0.0:9090");
    assert!(matches!(
        wide,
        Err(tgm_app::metrics::MetricsError::NotLoopback)
    ));
    let lan = parse_listen_addr("10.1.2.3:9090");
    assert!(matches!(
        lan,
        Err(tgm_app::metrics::MetricsError::NotLoopback)
    ));
    assert!(parse_listen_addr("not-an-address").is_err());
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn sample_reads_only_through_restricted_role_and_window_semantics(
    admin: PgPool,
) -> Result<(), sqlx::Error> {
    let (app, admin) = support::pools(admin).await;
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&app)
        .await?;
    assert_eq!(role, "app_user", "指标采集必须走受限角色");

    let tenant = support::seed_tenant(&admin, "metrics-tenant", 9201).await;
    let identity = support::seed_identity(&admin, tenant).await;

    // 窗口内：2 DEAD + 1 DONE → ratio 2/3；窗口外 1 DEAD 不计；CANCELLED 两边不计；
    // 2 PENDING 进队列深度。
    let now = "2026-11-26T04:00:00+00:00";
    seed_task(&admin, tenant, identity, "DEAD", now).await;
    seed_task(&admin, tenant, identity, "DEAD", now).await;
    seed_task(&admin, tenant, identity, "DONE", now).await;
    seed_task(
        &admin,
        tenant,
        identity,
        "DEAD",
        "2026-11-25T00:00:00+00:00",
    )
    .await;
    seed_task(&admin, tenant, identity, "CANCELLED", now).await;
    seed_task(&admin, tenant, identity, "PENDING", now).await;
    seed_task(&admin, tenant, identity, "PENDING", now).await;

    let mut watermark = None;
    let first = collect_sample(&app, &mut watermark, &config()).await?;
    assert!(first.latencies.is_empty(), "首个周期只推水位，不得回溯历史");
    assert_eq!(first.queue_depth, 2);
    assert_eq!(first.dead_ratio_by_tenant.len(), 1);
    let (tenant_id, ratio) = first.dead_ratio_by_tenant[0];
    assert_eq!(tenant_id, tenant.get());
    assert!(
        (ratio - 2.0 / 3.0).abs() < 1e-9,
        "窗口内 DEAD/(DONE+DEAD) 应为 2/3"
    );

    // 第二个周期：窗口内新增一条延迟样本（30 秒前落库），队列深度不变。
    seed_event_with_age(&admin, identity, 1, 30).await;
    let second = collect_sample(&app, &mut watermark, &config()).await?;
    assert_eq!(second.latencies.len(), 1);
    assert!(
        (second.latencies[0] - 30.0).abs() < 5.0,
        "延迟样本应约为 30 秒，实际 {}",
        second.latencies[0]
    );
    assert_eq!(second.queue_depth, 2);

    // 租户逻辑删除后：不出序列、不报错（app_user 看不到 deleted 租户，
    // list_all_active_tenants 只剩 status='active'）。
    sqlx::query("UPDATE tenants SET status = 'deleted' WHERE id = $1")
        .bind(tenant.get())
        .execute(admin.raw())
        .await?;
    let third = collect_sample(&app, &mut watermark, &config()).await?;
    assert!(third.dead_ratio_by_tenant.is_empty());
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn empty_denominator_yields_zero_and_truncation_caps_tenant_series(
    admin: PgPool,
) -> Result<(), sqlx::Error> {
    let (app, admin) = support::pools(admin).await;
    let first_tenant = support::seed_tenant(&admin, "metrics-empty", 9202).await;
    let identity = support::seed_identity(&admin, first_tenant).await;
    // 只有 PENDING：分母为零，ratio 记 0 而不是除零。
    seed_task(
        &admin,
        first_tenant,
        identity,
        "PENDING",
        "2026-11-26T04:00:00+00:00",
    )
    .await;

    let mut watermark = None;
    let mut bounded = config();
    bounded.max_tenants = 1;
    let second_tenant = support::seed_tenant(&admin, "metrics-overflow", 9203).await;
    let sample = collect_sample(&app, &mut watermark, &bounded).await?;
    assert!(sample.tenant_truncated, "超过基数上限必须截断");
    assert_eq!(
        sample.dead_ratio_by_tenant.len(),
        1,
        "截断后租户级序列不得超过上限"
    );
    assert_eq!(sample.dead_ratio_by_tenant[0].0, first_tenant.get());
    assert_eq!(sample.dead_ratio_by_tenant[0].1, 0.0);
    assert_eq!(sample.queue_depth, 1);
    let _ = second_tenant;
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn exposition_contains_only_frozen_names_and_tenant_label(
    admin: PgPool,
) -> Result<(), sqlx::Error> {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "metrics-render", 9204).await;
    let identity = support::seed_identity(&admin, tenant).await;
    seed_task(
        &admin,
        tenant,
        identity,
        "DEAD",
        "2026-11-26T04:00:00+00:00",
    )
    .await;

    // 水位线语义：先推一次水位，再播种延迟样本，下一周期才采得到 ——
    // 与生产里的时序一致（水位只追新落的行）。
    let mut watermark = None;
    let _ = collect_sample(&app, &mut watermark, &config()).await?;
    seed_event_with_age(&admin, identity, 1, 12).await;
    let sample = collect_sample(&app, &mut watermark, &config()).await?;
    assert_eq!(sample.latencies.len(), 1, "水位之后的新事件必须被采到");

    // 与 MetricsRuntime::bind 同形：直方图带冻结分桶，
    // 渲染出 bucket（le）形态而非默认分位（quantile）形态。
    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(tgm_app::metrics::INGEST_LATENCY_METRIC.to_owned()),
            tgm_app::metrics::INGEST_LATENCY_BUCKETS,
        )
        .expect("测试分桶配置必须合法")
        .build_recorder();
    record_sample(&recorder, &sample);
    recorder.handle().run_upkeep();
    let body = recorder.handle().render();

    for metric in [
        INGEST_LATENCY_METRIC,
        DELIVERY_DEAD_RATIO_METRIC,
        DELIVERY_QUEUE_DEPTH_METRIC,
        COLLECT_SUCCESS_METRIC,
    ] {
        assert!(body.contains(metric), "exposition 缺少 {metric}");
    }
    let tenant_label = format!("tenant_id=\"{}\"", tenant.get());
    assert!(body.contains(&tenant_label), "租户级序列必须带 tenant_id");
    // label 白名单之外不出现任何 label：逐行检查非注释序列行。
    for line in body.lines() {
        if line.starts_with('#') || !line.contains('{') {
            continue;
        }
        let labels = line
            .split_once('{')
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(labels, _)| labels)
            .expect("序列行必须含闭合花括号");
        for label in labels.split(',') {
            // tenant_id 是唯一业务 label；le / quantile 是 Prometheus
            // exposition 对直方图与分位摘要的格式标签，不是数据标签。
            assert!(
                label.starts_with("tenant_id=")
                    || label.starts_with("le=")
                    || label.starts_with("quantile="),
                "出现白名单外的 label：{label}"
            );
        }
    }

    // /metrics 经 axum 路由暴露，返回同一份 exposition。
    let response = router(recorder.handle())
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("构造请求失败"),
        )
        .await
        .expect("路由请求失败");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("读取响应失败");
    let rendered = String::from_utf8(bytes.to_vec()).expect("exposition 必须是 UTF-8");
    assert!(rendered.contains(INGEST_LATENCY_METRIC));
    assert!(rendered.contains(&tenant_label));
    Ok(())
}

#[sqlx::test(migrations = "../../migrations-test")]
async fn bind_rejects_non_loopback_and_serves_until_stop(admin: PgPool) -> Result<(), sqlx::Error> {
    let (app, admin) = support::pools(admin).await;
    let tenant = support::seed_tenant(&admin, "metrics-serve", 9205).await;
    let _identity = support::seed_identity(&admin, tenant).await;

    let wide = tgm_app::metrics::MetricsRuntime::bind(MetricsConfig {
        listen_addr: "0.0.0.0:0".parse::<SocketAddr>().expect("测试地址必须合法"),
        ..Default::default()
    })
    .await;
    assert!(
        matches!(wide, Err(tgm_app::metrics::MetricsError::NotLoopback)),
        "非回环绑定必须被拒"
    );

    let runtime = tgm_app::metrics::MetricsRuntime::bind(config())
        .await
        .expect("回环绑定应成功");
    let addr = runtime.local_addr().expect("读取绑定地址失败");
    assert!(addr.ip().is_loopback());

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        runtime
            .serve(app, async {
                let _ = stopped.await;
            })
            .await
    });
    // 等采集至少跑一轮（首周期只推水位），然后停。
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = stop.send(());
    task.await
        .expect("监听任务不应 panic")
        .expect("监听应干净退出");
    Ok(())
}
