use anyhow::{Context, Result};
use axum::{
    extract::{MatchedPath, Path, Query, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get},
    Json, Router,
};
use clap::Parser;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{error, info};

use serde::Deserialize;

use crate::metrics::{spawn_metrics_server, BackendMetrics};
use crate::{derive_forwarding_address, CreateForwardingRequest, ForwardingRequest};

/// Backend configuration
#[derive(Parser, Debug)]
pub struct BackendConfig {
    /// Port to listen on
    #[arg(long, env = "PORT", default_value = "8080")]
    pub port: u16,

    /// Path to database file
    #[arg(long, env = "DB_PATH", default_value = "storage/backend.db")]
    pub db_path: PathBuf,

    /// Address for the dedicated Prometheus metrics listener
    #[arg(long, env = "BACKEND_METRICS_BIND")]
    pub metrics_bind: Option<String>,
}

/// SQLite storage for backend forwarding requests
pub struct BackendStorage {
    conn: Arc<Mutex<Connection>>,
}

impl BackendStorage {
    /// Create or open backend database
    pub fn new(db_path: &StdPath) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {:?}", parent))?;
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("Failed to open backend DB at {:?}", db_path))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS forwarding_requests (
                forward_addr   TEXT PRIMARY KEY,
                dest_domain    INTEGER NOT NULL,
                dest_recipient TEXT NOT NULL,
                created_at     TEXT
            )",
            [],
        )
        .context("Failed to create forwarding_requests table")?;

        info!("Opened backend database at {:?}", db_path);

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Create a new forwarding request, or return the existing one for the address (idempotent).
    /// Returns `(request, true)` if newly created, `(request, false)` if an existing request was returned.
    pub fn create_request(
        &self,
        create_req: CreateForwardingRequest,
    ) -> Result<(ForwardingRequest, bool)> {
        let conn = self.conn.lock().unwrap();

        let created_at = chrono::Utc::now().to_rfc3339();

        conn.execute(
            "INSERT OR IGNORE INTO forwarding_requests (forward_addr, dest_domain, dest_recipient, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                &create_req.forward_addr,
                &create_req.dest_domain,
                &create_req.dest_recipient,
                &created_at
            ],
        )
        .context("Failed to insert forwarding request")?;

        let inserted = conn.changes() > 0;

        let request = if inserted {
            info!(
                "Created forwarding request for address {}",
                create_req.forward_addr
            );
            ForwardingRequest {
                forward_addr: create_req.forward_addr,
                dest_domain: create_req.dest_domain,
                dest_recipient: create_req.dest_recipient,
                created_at: Some(created_at),
            }
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT forward_addr, dest_domain, dest_recipient, created_at
                     FROM forwarding_requests WHERE forward_addr = ?1",
                )
                .context("Failed to prepare SELECT statement")?;

            let existing = stmt
                .query_row(params![&create_req.forward_addr], |row| {
                    Ok(ForwardingRequest {
                        forward_addr: row.get(0)?,
                        dest_domain: row.get(1)?,
                        dest_recipient: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                })
                .context("Failed to query existing request")?;

            info!(
                "Returning existing pending request for address {}",
                existing.forward_addr
            );
            existing
        };

        Ok((request, inserted))
    }

    /// Add a forwarding request directly to storage. Used by tests and setup helpers.
    pub fn add_request(&self, request: ForwardingRequest) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO forwarding_requests (forward_addr, dest_domain, dest_recipient, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                &request.forward_addr,
                &request.dest_domain,
                &request.dest_recipient,
                &request.created_at
            ],
        )
        .context("Failed to add forwarding request")?;

        Ok(())
    }

    /// Load all pending forwarding requests ordered by creation time.
    pub fn list_requests(&self) -> Result<Vec<ForwardingRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT forward_addr, dest_domain, dest_recipient, created_at
                 FROM forwarding_requests ORDER BY created_at",
            )
            .context("Failed to prepare SELECT statement")?;

        let rows = stmt
            .query_map([], |row| {
                Ok(ForwardingRequest {
                    forward_addr: row.get(0)?,
                    dest_domain: row.get(1)?,
                    dest_recipient: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .context("Failed to query forwarding requests")?;

        let mut requests = Vec::new();
        for row in rows {
            requests.push(row.context("Failed to read row")?);
        }

        Ok(requests)
    }

    /// Remove a pending request by forwarding address after forwarding completes.
    pub fn remove_by_addr(&self, forward_addr: &str) -> Result<Option<ForwardingRequest>> {
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT forward_addr, dest_domain, dest_recipient, created_at
                 FROM forwarding_requests WHERE forward_addr = ?1",
            )
            .context("Failed to prepare SELECT statement")?;

        let request = stmt
            .query_row(params![forward_addr], |row| {
                Ok(ForwardingRequest {
                    forward_addr: row.get(0)?,
                    dest_domain: row.get(1)?,
                    dest_recipient: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .optional()
            .context("Failed to query forwarding request")?;

        if request.is_some() {
            conn.execute(
                "DELETE FROM forwarding_requests WHERE forward_addr = ?1",
                params![forward_addr],
            )
            .context("Failed to delete forwarding request")?;
        }

        Ok(request)
    }
}

#[derive(Clone)]
pub struct BackendState {
    storage: Arc<BackendStorage>,
    metrics: Arc<BackendMetrics>,
}

impl BackendState {
    /// Create backend state backed by SQLite storage and initialize queue gauges.
    pub fn new(db_path: PathBuf, metrics: Arc<BackendMetrics>) -> Result<Self> {
        let storage = Arc::new(BackendStorage::new(&db_path)?);
        let state = Self { storage, metrics };
        state.refresh_queue_metrics()?;
        let count = state.list_requests()?.len();
        info!("Loaded {} pending requests from database", count);
        Ok(state)
    }

    /// Add a request directly to storage and refresh queue gauges.
    pub fn add_request(&self, request: ForwardingRequest) -> Result<()> {
        let result = self.storage.add_request(request);
        if result.is_ok() {
            self.refresh_queue_metrics()?;
        }
        result
    }

    /// Create a request via the idempotent storage path and update creation metrics.
    pub fn create_request(
        &self,
        create_req: CreateForwardingRequest,
    ) -> Result<(ForwardingRequest, bool)> {
        match self.storage.create_request(create_req) {
            Ok((request, created)) => {
                self.metrics
                    .record_create_result(if created { "created" } else { "duplicate" });
                self.refresh_queue_metrics()?;
                Ok((request, created))
            }
            Err(err) => {
                self.metrics.record_create_result("error");
                Err(err)
            }
        }
    }

    pub fn list_requests(&self) -> Result<Vec<ForwardingRequest>> {
        self.storage.list_requests()
    }

    /// Remove a request by address and update completion metrics.
    pub fn remove_by_addr(&self, forward_addr: &str) -> Result<Option<ForwardingRequest>> {
        match self.storage.remove_by_addr(forward_addr) {
            Ok(request) => {
                self.metrics.record_complete_result(if request.is_some() {
                    "removed"
                } else {
                    "not_found"
                });
                self.refresh_queue_metrics()?;
                Ok(request)
            }
            Err(err) => {
                self.metrics.record_complete_result("error");
                Err(err)
            }
        }
    }

    #[cfg(test)]
    pub fn render_metrics(&self) -> Result<String> {
        self.metrics.render()
    }

    fn refresh_queue_metrics(&self) -> Result<()> {
        let requests = self.storage.list_requests()?;
        self.metrics.update_queue_state(&requests);
        Ok(())
    }
}

/// Backend server
pub struct Backend {
    state: BackendState,
    port: u16,
    metrics_bind: Option<String>,
    metrics: Arc<BackendMetrics>,
}

impl Backend {
    /// Construct a backend with an optional dedicated metrics listener bind.
    pub fn new(port: u16, db_path: PathBuf, metrics_bind: Option<String>) -> Result<Self> {
        let metrics = Arc::new(BackendMetrics::new()?);
        let state = BackendState::new(db_path, metrics.clone())?;

        Ok(Self {
            state,
            port,
            metrics_bind,
            metrics,
        })
    }

    pub fn state(&self) -> BackendState {
        self.state.clone()
    }

    /// Build the public backend API router with HTTP metrics middleware attached.
    pub(crate) fn app(&self) -> Router {
        Router::new()
            .route("/forwarding-address", get(get_forwarding_address))
            .route(
                "/forwarding-requests",
                get(list_requests).post(create_request),
            )
            .route("/forwarding-requests/:addr", delete(complete_request))
            .layer(middleware::from_fn_with_state(
                self.metrics.clone(),
                track_backend_http_metrics,
            ))
            .with_state(self.state.clone())
    }

    #[cfg(test)]
    pub(crate) fn metrics_app(&self) -> Router {
        crate::metrics::metrics_router(self.metrics.registry())
    }

    /// Start the backend API server and the dedicated metrics server.
    pub async fn serve(self) -> Result<()> {
        let metrics_bind = self.metrics_bind.clone();
        if let Some(bind) = metrics_bind.as_deref() {
            let _metrics_server = spawn_metrics_server(bind, self.metrics.registry()).await?;
            info!("Backend metrics listening on {}", bind);
        }

        let addr = format!("0.0.0.0:{}", self.port);
        info!("Backend listening on {}", addr);

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, self.app()).await?;

        Ok(())
    }
}

async fn track_backend_http_metrics(
    State(metrics): State<Arc<BackendMetrics>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    let method = request.method().as_str().to_string();
    let start = Instant::now();
    let response = next.run(request).await;

    metrics.observe_http(&route, &method, response.status().as_u16(), start.elapsed());
    response
}

/// Query params for `GET /forwarding-address`.
#[derive(Deserialize)]
struct ForwardingAddressQuery {
    dest_domain: u32,
    dest_recipient: String,
}

/// `GET /forwarding-address?dest_domain=<u32>&dest_recipient=<hex>` derives a forwarding address.
async fn get_forwarding_address(Query(params): Query<ForwardingAddressQuery>) -> impl IntoResponse {
    match derive_forwarding_address(params.dest_domain, &params.dest_recipient) {
        Ok(address) => (
            StatusCode::OK,
            Json(serde_json::json!({ "address": address })),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to derive forwarding address: {:#}", e);
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

/// `GET /forwarding-requests` lists all pending forwarding requests.
async fn list_requests(
    State(state): State<BackendState>,
) -> Result<Json<Vec<ForwardingRequest>>, StatusCode> {
    match state.list_requests() {
        Ok(requests) => Ok(Json(requests)),
        Err(e) => {
            error!("Failed to list requests: {:#}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `POST /forwarding-requests` creates a request, or returns the existing request for the same address.
async fn create_request(
    State(state): State<BackendState>,
    Json(create_req): Json<CreateForwardingRequest>,
) -> impl IntoResponse {
    match state.create_request(create_req) {
        Ok((request, created)) => {
            let status_code = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };

            (status_code, Json(request)).into_response()
        }
        Err(e) => {
            error!("Failed to create forwarding request: {:#}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `DELETE /forwarding-requests/:addr` removes a completed forwarding request.
async fn complete_request(
    Path(addr): Path<String>,
    State(state): State<BackendState>,
) -> impl IntoResponse {
    match state.remove_by_addr(&addr) {
        Ok(Some(request)) => {
            info!("Removed completed request for address {}", addr);
            (StatusCode::OK, Json(request)).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            error!("Failed to remove request for {}: {:#}", addr, e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_backend() -> Backend {
        let db_path = std::env::temp_dir().join(format!(
            "forwarding-backend-{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Backend::new(0, db_path, Some("127.0.0.1:0".to_string())).unwrap()
    }

    #[tokio::test]
    async fn metrics_endpoint_renders_prometheus_output() {
        let backend = test_backend();

        let response = backend
            .metrics_app()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn create_and_complete_requests_update_metrics() {
        let backend = test_backend();
        let app = backend.app();

        let body = serde_json::to_vec(&CreateForwardingRequest {
            forward_addr: "celestia1test1".to_string(),
            dest_domain: 42161,
            dest_recipient: "0x000000000000000000000000742d35Cc6634C0532925a3b844Bc9e7595f00000"
                .to_string(),
        })
        .unwrap();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/forwarding-requests")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let duplicate_body = serde_json::to_vec(&CreateForwardingRequest {
            forward_addr: "celestia1test1".to_string(),
            dest_domain: 42161,
            dest_recipient: "0x000000000000000000000000742d35Cc6634C0532925a3b844Bc9e7595f00000"
                .to_string(),
        })
        .unwrap();

        let duplicate = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/forwarding-requests")
                    .header("content-type", "application/json")
                    .body(Body::from(duplicate_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(duplicate.status(), StatusCode::OK);

        let deleted = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/forwarding-requests/celestia1test1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::OK);

        let metrics = backend.state().render_metrics().unwrap();
        assert!(metrics.contains("forwarding_backend_requests_created_total{result=\"created\"} 1"));
        assert!(
            metrics.contains("forwarding_backend_requests_created_total{result=\"duplicate\"} 1")
        );
        assert!(
            metrics.contains("forwarding_backend_requests_completed_total{result=\"removed\"} 1")
        );
        assert!(metrics.contains("forwarding_backend_pending_requests 0"));
        assert!(metrics.contains("forwarding_backend_http_requests_total"));
    }
}
