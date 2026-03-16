use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use chrono::{DateTime, Utc};
use prometheus::{
    histogram_opts, opts, Encoder, Gauge, GaugeVec, HistogramVec, IntCounterVec, IntGauge,
    Registry, TextEncoder,
};
use std::time::Duration;
use tracing::error;

use crate::{Balance, ForwardingRequest};

#[derive(Clone)]
struct MetricsState {
    registry: Registry,
}

async fn render_metrics(State(state): State<MetricsState>) -> impl IntoResponse {
    match encode_registry(&state.registry) {
        Ok(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                TextEncoder::new().format_type().to_string(),
            )],
            body,
        )
            .into_response(),
        Err(err) => {
            error!("Failed to encode metrics: {err:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub fn metrics_router(registry: Registry) -> Router {
    Router::new()
        .route("/metrics", get(render_metrics))
        .with_state(MetricsState { registry })
}

pub async fn spawn_metrics_server(
    bind: &str,
    registry: Registry,
) -> Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("Failed to bind metrics listener on {bind}"))?;
    let app = metrics_router(registry);

    Ok(tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            error!("Metrics server stopped: {err:#}");
        }
    }))
}

pub fn encode_registry(registry: &Registry) -> Result<String> {
    let metric_families = registry.gather();
    let mut buffer = Vec::new();
    TextEncoder::new()
        .encode(&metric_families, &mut buffer)
        .context("Failed to encode Prometheus metrics")?;
    String::from_utf8(buffer).context("Metrics output was not valid UTF-8")
}

fn duration_seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

fn unix_timestamp_now() -> f64 {
    Utc::now().timestamp() as f64
}

pub fn classify_error(err: &anyhow::Error) -> &'static str {
    let text = err.to_string().to_ascii_lowercase();

    if text.contains("timed out") || text.contains("timeout") {
        "timeout"
    } else if text.contains("parse") {
        "parse_error"
    } else if text.contains("grpc") || text.contains("tonic") {
        "grpc_error"
    } else if text.contains("http") || text.contains("backend returned error") {
        "http_error"
    } else if text.contains("transport") || text.contains("connect") {
        "transport_error"
    } else {
        "error"
    }
}

pub struct BackendMetrics {
    registry: Registry,
    http_requests_total: IntCounterVec,
    http_request_duration_seconds: HistogramVec,
    pending_requests: IntGauge,
    oldest_pending_request_age_seconds: Gauge,
    requests_created_total: IntCounterVec,
    requests_completed_total: IntCounterVec,
}

impl BackendMetrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new();

        let http_requests_total = IntCounterVec::new(
            opts!(
                "forwarding_backend_http_requests_total",
                "Total HTTP requests handled by the backend"
            ),
            &["route", "method", "status_code"],
        )?;
        let http_request_duration_seconds = HistogramVec::new(
            histogram_opts!(
                "forwarding_backend_http_request_duration_seconds",
                "Duration of backend HTTP requests"
            ),
            &["route", "method", "status_code"],
        )?;
        let pending_requests = IntGauge::new(
            "forwarding_backend_pending_requests",
            "Current number of pending forwarding requests",
        )?;
        let oldest_pending_request_age_seconds = Gauge::new(
            "forwarding_backend_oldest_pending_request_age_seconds",
            "Age in seconds of the oldest pending forwarding request",
        )?;
        let requests_created_total = IntCounterVec::new(
            opts!(
                "forwarding_backend_requests_created_total",
                "Total backend request creation attempts"
            ),
            &["result"],
        )?;
        let requests_completed_total = IntCounterVec::new(
            opts!(
                "forwarding_backend_requests_completed_total",
                "Total backend request completion attempts"
            ),
            &["result"],
        )?;

        registry.register(Box::new(http_requests_total.clone()))?;
        registry.register(Box::new(http_request_duration_seconds.clone()))?;
        registry.register(Box::new(pending_requests.clone()))?;
        registry.register(Box::new(oldest_pending_request_age_seconds.clone()))?;
        registry.register(Box::new(requests_created_total.clone()))?;
        registry.register(Box::new(requests_completed_total.clone()))?;

        Ok(Self {
            registry,
            http_requests_total,
            http_request_duration_seconds,
            pending_requests,
            oldest_pending_request_age_seconds,
            requests_created_total,
            requests_completed_total,
        })
    }

    pub fn registry(&self) -> Registry {
        self.registry.clone()
    }

    pub fn observe_http(&self, route: &str, method: &str, status_code: u16, duration: Duration) {
        let status_code = status_code.to_string();
        self.http_requests_total
            .with_label_values(&[route, method, &status_code])
            .inc();
        self.http_request_duration_seconds
            .with_label_values(&[route, method, &status_code])
            .observe(duration_seconds(duration));
    }

    pub fn record_create_result(&self, result: &str) {
        self.requests_created_total
            .with_label_values(&[result])
            .inc();
    }

    pub fn record_complete_result(&self, result: &str) {
        self.requests_completed_total
            .with_label_values(&[result])
            .inc();
    }

    pub fn update_queue_state(&self, requests: &[ForwardingRequest]) {
        self.pending_requests.set(requests.len() as i64);

        let oldest_age_seconds = requests
            .iter()
            .filter_map(|request| request.created_at.as_deref())
            .filter_map(parse_rfc3339)
            .map(|created_at| (Utc::now() - created_at).num_seconds().max(0) as f64)
            .max_by(|left, right| left.total_cmp(right))
            .unwrap_or(0.0);

        self.oldest_pending_request_age_seconds
            .set(oldest_age_seconds);
    }

    #[cfg(test)]
    pub fn render(&self) -> Result<String> {
        encode_registry(&self.registry)
    }
}

pub struct RelayerMetrics {
    registry: Registry,
    rounds_total: IntCounterVec,
    round_duration_seconds: HistogramVec,
    requests_fetched: IntGauge,
    last_successful_round_timestamp_seconds: Gauge,
    backend_calls_total: IntCounterVec,
    celestia_calls_total: IntCounterVec,
    forwarding_attempts_total: IntCounterVec,
    forwarding_attempt_duration_seconds: HistogramVec,
    last_successful_forward_timestamp_seconds: Gauge,
    signer_balance: GaugeVec,
    igp_fee_quote_utia: GaugeVec,
}

impl RelayerMetrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new();

        let rounds_total = IntCounterVec::new(
            opts!(
                "forwarding_relayer_rounds_total",
                "Total relayer processing rounds"
            ),
            &["result"],
        )?;
        let round_duration_seconds = HistogramVec::new(
            histogram_opts!(
                "forwarding_relayer_round_duration_seconds",
                "Duration of relayer processing rounds"
            ),
            &["result"],
        )?;
        let requests_fetched = IntGauge::new(
            "forwarding_relayer_requests_fetched",
            "Number of requests fetched during the latest successful backend poll",
        )?;
        let last_successful_round_timestamp_seconds = Gauge::new(
            "forwarding_relayer_last_successful_round_timestamp_seconds",
            "Unix timestamp of the last successful relayer round",
        )?;
        let backend_calls_total = IntCounterVec::new(
            opts!(
                "forwarding_relayer_backend_calls_total",
                "Total backend calls made by the relayer"
            ),
            &["operation", "result"],
        )?;
        let celestia_calls_total = IntCounterVec::new(
            opts!(
                "forwarding_relayer_celestia_calls_total",
                "Total Celestia calls made by the relayer"
            ),
            &["operation", "result"],
        )?;
        let forwarding_attempts_total = IntCounterVec::new(
            opts!(
                "forwarding_relayer_forwarding_attempts_total",
                "Total forwarding submissions attempted by the relayer"
            ),
            &["result"],
        )?;
        let forwarding_attempt_duration_seconds = HistogramVec::new(
            histogram_opts!(
                "forwarding_relayer_forwarding_attempt_duration_seconds",
                "Duration of forwarding submissions"
            ),
            &["result"],
        )?;
        let last_successful_forward_timestamp_seconds = Gauge::new(
            "forwarding_relayer_last_successful_forward_timestamp_seconds",
            "Unix timestamp of the last successful forwarding submission",
        )?;
        let signer_balance = GaugeVec::new(
            opts!(
                "forwarding_relayer_signer_balance",
                "Current relayer signer balance by denom"
            ),
            &["denom"],
        )?;
        let igp_fee_quote_utia = GaugeVec::new(
            opts!(
                "forwarding_relayer_igp_fee_quote_utia",
                "Latest quoted IGP fee in utia by destination domain"
            ),
            &["dest_domain"],
        )?;

        registry.register(Box::new(rounds_total.clone()))?;
        registry.register(Box::new(round_duration_seconds.clone()))?;
        registry.register(Box::new(requests_fetched.clone()))?;
        registry.register(Box::new(last_successful_round_timestamp_seconds.clone()))?;
        registry.register(Box::new(backend_calls_total.clone()))?;
        registry.register(Box::new(celestia_calls_total.clone()))?;
        registry.register(Box::new(forwarding_attempts_total.clone()))?;
        registry.register(Box::new(forwarding_attempt_duration_seconds.clone()))?;
        registry.register(Box::new(last_successful_forward_timestamp_seconds.clone()))?;
        registry.register(Box::new(signer_balance.clone()))?;
        registry.register(Box::new(igp_fee_quote_utia.clone()))?;

        Ok(Self {
            registry,
            rounds_total,
            round_duration_seconds,
            requests_fetched,
            last_successful_round_timestamp_seconds,
            backend_calls_total,
            celestia_calls_total,
            forwarding_attempts_total,
            forwarding_attempt_duration_seconds,
            last_successful_forward_timestamp_seconds,
            signer_balance,
            igp_fee_quote_utia,
        })
    }

    pub fn registry(&self) -> Registry {
        self.registry.clone()
    }

    pub fn observe_round(&self, result: &str, duration: Duration) {
        self.rounds_total.with_label_values(&[result]).inc();
        self.round_duration_seconds
            .with_label_values(&[result])
            .observe(duration_seconds(duration));
    }

    pub fn set_requests_fetched(&self, count: usize) {
        self.requests_fetched.set(count as i64);
    }

    pub fn mark_successful_round(&self) {
        self.last_successful_round_timestamp_seconds
            .set(unix_timestamp_now());
    }

    pub fn observe_backend_call(&self, operation: &str, result: &str) {
        self.backend_calls_total
            .with_label_values(&[operation, result])
            .inc();
    }

    pub fn observe_celestia_call(&self, operation: &str, result: &str) {
        self.celestia_calls_total
            .with_label_values(&[operation, result])
            .inc();
    }

    pub fn observe_forwarding_attempt(&self, result: &str, duration: Duration) {
        self.forwarding_attempts_total
            .with_label_values(&[result])
            .inc();
        self.forwarding_attempt_duration_seconds
            .with_label_values(&[result])
            .observe(duration_seconds(duration));
    }

    pub fn mark_successful_forward(&self) {
        self.last_successful_forward_timestamp_seconds
            .set(unix_timestamp_now());
    }

    pub fn update_signer_balance(&self, balances: &[Balance]) {
        for balance in balances {
            if let Ok(amount) = balance.amount.parse::<f64>() {
                self.signer_balance
                    .with_label_values(&[&balance.denom])
                    .set(amount);
            }
        }
    }

    pub fn set_igp_fee_quote(&self, dest_domain: u32, amount: u64) {
        let dest_domain = dest_domain.to_string();
        self.igp_fee_quote_utia
            .with_label_values(&[&dest_domain])
            .set(amount as f64);
    }
}

fn parse_rfc3339(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date_time| date_time.with_timezone(&Utc))
}
