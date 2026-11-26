//! 运行指标（G5 提案 §2.1）：三个核心指标全部从库里取（`ops/01` §3.1），
//! 不从进程内计数器取 —— 进程重启后内存计数归零，而这三件事恰恰要在重启后依然可见。
//!
//! 边界（提案 §2.1 / `ops/00` §三）：
//!
//! - `/metrics` 由 control-plane 在**独立私有监听**上暴露，与业务 HTTP 分开；
//!   只接受回环地址，非回环在启动期被拒（过宽配置 = 拒绝启动，失败方向吵闹）。
//! - 读取只走既有受限角色 `app_user` 的既有权限：`tenants` 无 RLS 可直查，
//!   `delivery_tasks` 逐租户经 `tenant_tx`（RLS 策略内），`canonical_events`
//!   是丙类不启用 RLS。不加任何 GRANT / RLS 例外 / 新角色（提案 §五 停止条件）。
//! - 指标采集失败不阻塞业务启动、不改变投递语义：采集在独立任务里，
//!   失败只记脱敏 warn 并把 `tgm_metrics_collect_success` 置 0。
//! - 本片不启用生产抓取与告警渠道：未设置 `TGM_METRICS_LISTEN_ADDR` 时
//!   不安装 recorder、不起监听，control-plane 照常启动。
//!
//! 冻结的指标清单（名称 / 单位 / label 白名单，提案 §四 判据）：
//!
//! | 名称 | 形态 | 单位 | label |
//! |---|---|---|---|
//! | `tgm_ingest_latency_seconds` | histogram | 秒 | 无 |
//! | `tgm_delivery_dead_ratio` | gauge | 0..1 比例 | `tenant_id` |
//! | `tgm_delivery_queue_depth` | gauge | 任务数 | 无 |
//! | `tgm_metrics_collect_success` | gauge | 0/1 | 无 |
//!
//! label 只有 `tenant_id`（数据库 bigint，不是名称/正文/凭据），
//! 基数上界 = 活跃租户数且受 `max_tenants` 截断；消息正文、关键词、凭据、
//! session、源链接一律不进 label 与 exposition。

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use metrics_exporter_prometheus::{
    Matcher, PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
};
use sqlx::PgPool;
use sqlx::types::chrono::{DateTime, Utc};
use tgm_core::ids::TenantId;
use tgm_db::repo::list_all_active_tenants;
use tgm_db::tenant::tenant_tx;
use tokio::net::TcpListener;
use tokio::sync::watch;

pub const INGEST_LATENCY_METRIC: &str = "tgm_ingest_latency_seconds";
pub const DELIVERY_DEAD_RATIO_METRIC: &str = "tgm_delivery_dead_ratio";
pub const DELIVERY_QUEUE_DEPTH_METRIC: &str = "tgm_delivery_queue_depth";
pub const COLLECT_SUCCESS_METRIC: &str = "tgm_metrics_collect_success";

/// 采集延迟直方图分桶（秒）。告警阈值是 P99 > 60s 持续 5 分钟（`ops/01` §3.1），
/// 60s 两侧都要有足够的桶分辨率。
pub const INGEST_LATENCY_BUCKETS: &[f64] = &[
    1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("TGM_METRICS_LISTEN_ADDR 不是合法监听地址：{0}")]
    Invalid(String),
    #[error("metrics 监听地址必须是回环地址（/metrics 不对公网暴露）")]
    NotLoopback,
    #[error("metrics recorder 构建失败：{0}")]
    Recorder(String),
    #[error("metrics 监听绑定失败：{0}")]
    Bind(#[from] std::io::Error),
}

/// 解析并校验 `TGM_METRICS_LISTEN_ADDR`：合法 SocketAddr 且必须是回环。
/// 未设置由调用方处理（不启用指标）；设置但非法 = 拒绝启动。
pub fn parse_listen_addr(raw: &str) -> Result<SocketAddr, MetricsError> {
    let addr: SocketAddr = raw
        .parse()
        .map_err(|_| MetricsError::Invalid(raw.to_owned()))?;
    if !addr.ip().is_loopback() {
        return Err(MetricsError::NotLoopback);
    }
    Ok(addr)
}

/// 指标采集与暴露的运行参数。
#[derive(Debug, Clone)]
pub struct MetricsConfig {
    pub listen_addr: SocketAddr,
    /// 采集周期。`ops/01` §3.1 的告警窗口以分钟计，30s 周期足够。
    pub collect_interval: Duration,
    /// 投递失败率的滚动窗口（`ops/01` §3.1：1 小时内 DEAD 比例）。
    pub failure_window: Duration,
    /// 租户级序列的基数上限：超过则截断（按租户 id 升序保留前 N 个），
    /// 失败方向是丢粒度而不是放任 label 基数膨胀。
    pub max_tenants: usize,
    /// 单周期采集延迟样本上限，防突发积压把采集周期拖垮。
    pub latency_sample_cap: i64,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:9090".parse().unwrap_or_else(|_| unreachable!()),
            collect_interval: Duration::from_secs(30),
            failure_window: Duration::from_secs(3600),
            max_tenants: 1000,
            latency_sample_cap: 10_000,
        }
    }
}

/// 一个采集周期的样本。
#[derive(Debug, Default)]
pub struct Sample {
    /// 消息在 Telegram 的时间 → 落 `canonical_events` 的时间（秒）。
    pub latencies: Vec<f64>,
    /// 全平台 `PENDING` 投递任务数。
    pub queue_depth: i64,
    /// 每个租户窗口内 DEAD/(DONE+DEAD)；分母为零记 0。
    pub dead_ratio_by_tenant: Vec<(i64, f64)>,
    /// 活跃租户数超过 `max_tenants`，租户级序列被截断。
    pub tenant_truncated: bool,
}

/// 从库中采一个周期的样本。只读、只走 `app_user` 既有权限。
///
/// 采集延迟的水位线语义：`watermark` 初始为 `None`，首个周期只把水位推到当前
/// 时间、不出样本（重启后不回溯历史，避免把停机积压误当实时延迟）；
/// 其后每个周期采 `(上次水位, 当前库时间]` 内新落的行。
pub async fn collect_sample(
    pool: &PgPool,
    watermark: &mut Option<DateTime<Utc>>,
    config: &MetricsConfig,
) -> Result<Sample, sqlx::Error> {
    let mut sample = Sample::default();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await?;

    if let Some(previous) = *watermark {
        sample.latencies = sqlx::query_scalar::<_, f64>(
            "SELECT extract(epoch FROM (created_at - message_date))::double precision
               FROM canonical_events
              WHERE created_at > $1 AND created_at <= $2
              ORDER BY id
              LIMIT $3",
        )
        .bind(previous)
        .bind(now)
        .bind(config.latency_sample_cap)
        .fetch_all(pool)
        .await?;
    }
    *watermark = Some(now);

    let mut tenants = list_all_active_tenants(pool).await?;
    if tenants.len() > config.max_tenants {
        tenants.truncate(config.max_tenants);
        sample.tenant_truncated = true;
    }
    // 窗口起点在库里算（`clock_timestamp() - N 秒`）：客户端没有经
    // `sqlx::types::chrono` 拿到 `TimeDelta`，且服务端时钟与查询时点一致。
    let window_seconds = i64::try_from(config.failure_window.as_secs()).unwrap_or(i64::MAX);
    for tenant in tenants {
        match tenant_stats(pool, tenant, now, window_seconds).await {
            Ok((pending, dead, terminal)) => {
                sample.queue_depth += pending;
                let ratio = if terminal > 0 {
                    dead as f64 / terminal as f64
                } else {
                    0.0
                };
                sample.dead_ratio_by_tenant.push((tenant.get(), ratio));
            }
            // 租户在周期间被停用/注销：跳过，不当作采集失败（与 rule worker 同形）。
            Err(sqlx::Error::Protocol(message)) if message == tgm_db::lifecycle::INACTIVE => {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(sample)
}

/// 单租户一次查询取出三个数：`PENDING` 任务数、窗口内进入 DEAD 的任务数、
/// 窗口内到达终态（DONE+DEAD）的任务数。CANCELLED 既非成功也非失败，两边都不计。
///
/// `completed_at` 在 DONE/DEAD/CANCELLED 转换时回填（`repo` 的失败与成功更新路径），
/// 所以「窗口内到达终态」用它而不是 `updated_at`。
///
/// ponytail: 逐租户全表聚合，单租户任务行超过百万时改 (tenant_id, status, completed_at)
/// 部分索引或预聚合。
async fn tenant_stats(
    pool: &PgPool,
    tenant: TenantId,
    now: DateTime<Utc>,
    window_seconds: i64,
) -> Result<(i64, i64, i64), sqlx::Error> {
    tenant_tx(pool, tenant, |conn| {
        Box::pin(async move {
            sqlx::query_as::<_, (i64, i64, i64)>(
                "SELECT count(*) FILTER (WHERE status = 'PENDING'),
                        count(*) FILTER (WHERE status = 'DEAD'
                                          AND completed_at > $1 - make_interval(secs => $2)),
                        count(*) FILTER (WHERE status IN ('DONE', 'DEAD')
                                          AND completed_at > $1 - make_interval(secs => $2))
                   FROM delivery_tasks",
            )
            .bind(now)
            .bind(window_seconds)
            .fetch_one(conn)
            .await
        })
    })
    .await
}

/// 把样本记进 recorder。只碰冻结清单里的四个名称；`tenant_id` 是唯一 label。
pub fn record_sample(recorder: &dyn metrics::Recorder, sample: &Sample) {
    metrics::with_local_recorder(recorder, || {
        for latency in &sample.latencies {
            metrics::histogram!(INGEST_LATENCY_METRIC).record(*latency);
        }
        metrics::gauge!(DELIVERY_QUEUE_DEPTH_METRIC).set(sample.queue_depth as f64);
        for (tenant_id, ratio) in &sample.dead_ratio_by_tenant {
            metrics::gauge!(DELIVERY_DEAD_RATIO_METRIC, "tenant_id" => tenant_id.to_string())
                .set(*ratio);
        }
        metrics::gauge!(COLLECT_SUCCESS_METRIC).set(1.0);
    });
}

/// 采集失败只落健康位，不动业务序列 —— 把失败的周期渲染成「队列深度 0」
/// 比缺数更危险。
pub fn record_collect_failure(recorder: &dyn metrics::Recorder) {
    metrics::with_local_recorder(recorder, || {
        metrics::gauge!(COLLECT_SUCCESS_METRIC).set(0.0);
    });
}

fn describe_all(recorder: &dyn metrics::Recorder) {
    metrics::with_local_recorder(recorder, || {
        metrics::describe_histogram!(
            INGEST_LATENCY_METRIC,
            metrics::Unit::Seconds,
            "消息在 Telegram 的时间到落 canonical_events 的延迟（水位线按采集周期推进，重启后不回溯）"
        );
        metrics::describe_gauge!(
            DELIVERY_DEAD_RATIO_METRIC,
            metrics::Unit::Count,
            "每租户 1 小时滚动窗口内 DEAD/(DONE+DEAD)；分母为零记 0；唯一 label 是 tenant_id"
        );
        metrics::describe_gauge!(
            DELIVERY_QUEUE_DEPTH_METRIC,
            metrics::Unit::Count,
            "delivery_tasks 中 PENDING 的行数（全平台求和）"
        );
        metrics::describe_gauge!(
            COLLECT_SUCCESS_METRIC,
            metrics::Unit::Count,
            "最近一个采集周期是否成功（1/0）"
        );
    });
}

/// 指标运行时：私有监听上的 `/metrics` + 后台采集任务。
///
/// `bind` 在业务服务启动前完成 —— 绑定失败（端口被占、地址非法）直接拒绝启动，
/// 失败方向是吵闹的。
pub struct MetricsRuntime {
    recorder: PrometheusRecorder,
    handle: PrometheusHandle,
    listener: TcpListener,
    config: MetricsConfig,
}

impl MetricsRuntime {
    pub async fn bind(config: MetricsConfig) -> Result<Self, MetricsError> {
        // bind 前复查回环：配置解析与这里任何一处失守都不放行。
        if !config.listen_addr.ip().is_loopback() {
            return Err(MetricsError::NotLoopback);
        }
        let recorder = PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Full(INGEST_LATENCY_METRIC.to_owned()),
                INGEST_LATENCY_BUCKETS,
            )
            .map_err(|error| MetricsError::Recorder(error.to_string()))?
            .build_recorder();
        let handle = recorder.handle();
        // 先用标准库绑（此时还没有进入必须异步的上下文），失败 = 拒绝启动。
        let std_listener = StdTcpListener::bind(config.listen_addr)?;
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener)?;
        Ok(Self {
            recorder,
            handle,
            listener,
            config,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 起采集任务与 `/metrics` 监听，直到收到停止信号。
    /// 采集失败不影响监听；监听错误返回给调用方记日志，不波及业务服务。
    pub async fn serve(
        self,
        pool: PgPool,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        describe_all(&self.recorder);
        let (stop, mut stop_rx) = watch::channel(false);
        let collector = tokio::spawn(collector_loop(
            pool,
            self.recorder,
            self.config.clone(),
            stop_rx.clone(),
        ));
        // `build_recorder` 不自带 upkeep（文档明说调用方负责）：不跑它，
        // 陈旧序列与分位分布会无界增长。60s 一次，与上游默认一致。
        let upkeep_handle = self.handle.clone();
        let upkeep = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                tokio::select! {
                    changed = stop_rx.changed() => {
                        if changed.is_err() || *stop_rx.borrow() {
                            break;
                        }
                    }
                    _ = interval.tick() => upkeep_handle.run_upkeep(),
                }
            }
        });
        let app = router(self.handle.clone());
        let served = axum::serve(self.listener, app)
            .with_graceful_shutdown(shutdown)
            .await;
        let _ = stop.send(true);
        // stop 已发，采集任务至多再跑完当前周期；收掉它再退出。
        let _ = collector.await;
        let _ = upkeep.await;
        served
    }
}

/// `/metrics` 路由。单独暴露是为了让测试不经监听就能驱动（tower oneshot）。
pub fn router(handle: PrometheusHandle) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(handle)
}

async fn metrics_handler(State(handle): State<PrometheusHandle>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        handle.render(),
    )
}

async fn collector_loop(
    pool: PgPool,
    recorder: PrometheusRecorder,
    config: MetricsConfig,
    mut stop: watch::Receiver<bool>,
) {
    let mut watermark: Option<DateTime<Utc>> = None;
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep(config.collect_interval) => {}
        }
        match collect_sample(&pool, &mut watermark, &config).await {
            Ok(sample) => {
                if sample.tenant_truncated {
                    tracing::warn!(
                        max_tenants = config.max_tenants,
                        "活跃租户数超过指标基数上限，租户级序列已截断"
                    );
                }
                record_sample(&recorder, &sample);
            }
            Err(error) => {
                tracing::warn!(error = %error, "指标采集周期失败，不影响业务");
                record_collect_failure(&recorder);
            }
        }
    }
}
