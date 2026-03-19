use anyhow::{anyhow, Context, Result};
use axum::{routing::get, Router};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;
use std::time::Duration;
use tracing::info;

const METRICS_UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

static METRICS_RUNTIME: OnceLock<MetricsRuntime> = OnceLock::new();

struct MetricsRuntime {
    handle: PrometheusHandle,
    port: u16,
}

pub fn init_metrics_exporter(port: Option<u16>) -> Result<()> {
    let Some(port) = port else {
        return Ok(());
    };

    if let Some(runtime) = METRICS_RUNTIME.get() {
        if runtime.port == port {
            return Ok(());
        }

        return Err(anyhow!(
            "metrics exporter already initialized on port {}",
            runtime.port
        ));
    }

    let handle = PrometheusBuilder::new()
        .install_recorder()
        .context("Failed to install Prometheus recorder")?;

    spawn_metrics_server(port, handle.clone());
    spawn_metrics_upkeep(handle.clone());

    METRICS_RUNTIME
        .set(MetricsRuntime { handle, port })
        .map_err(|_| anyhow!("metrics exporter initialized concurrently"))?;

    Ok(())
}

pub fn metrics_enabled() -> bool {
    METRICS_RUNTIME.get().is_some()
}

pub fn render_metrics() -> Option<String> {
    METRICS_RUNTIME.get().map(|runtime| runtime.handle.render())
}

fn spawn_metrics_server(port: u16, handle: PrometheusHandle) {
    tokio::spawn(async move {
        async fn metrics_handler(handle: PrometheusHandle) -> String {
            handle.render()
        }

        let app = Router::new().route(
            "/metrics",
            get({
                let handle = handle.clone();
                move || metrics_handler(handle.clone())
            }),
        );

        let addr = format!("0.0.0.0:{port}");
        info!("Metrics listening on http://{addr}/metrics");

        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                if let Err(err) = axum::serve(listener, app).await {
                    tracing::error!("Metrics server failed: {err:#}");
                }
            }
            Err(err) => {
                tracing::error!("Failed to bind metrics listener on {addr}: {err:#}");
            }
        }
    });
}

fn spawn_metrics_upkeep(handle: PrometheusHandle) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(METRICS_UPKEEP_INTERVAL).await;
            handle.run_upkeep();
        }
    });
}
