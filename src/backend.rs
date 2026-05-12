use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get},
    Json, Router,
};
use clap::Parser;
use metrics::{counter, gauge};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info};

use serde::Deserialize;

use crate::{derive_forwarding_address, CreateForwardingRequest, ForwardingRequest};

const BACKEND_METRICS_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Backend configuration
#[derive(Parser, Debug)]
pub struct BackendConfig {
    /// Port to listen on
    #[arg(long, env = "PORT", default_value = "8080")]
    pub port: u16,

    /// Path to database file
    #[arg(long, env = "DB_PATH", default_value = "storage/backend.db")]
    pub db_path: PathBuf,

    /// Metrics port for Prometheus scraping
    #[arg(long, env = "BACKEND_METRICS_PORT")]
    pub metrics_port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct PendingRequestMetricsSnapshot {
    pub pending_requests: usize,
    pub oldest_created_at: Option<String>,
}

/// SQLite storage for backend forwarding requests
pub struct BackendStorage {
    conn: Arc<Mutex<Connection>>,
}

impl BackendStorage {
    /// Create or open backend database
    pub fn new(db_path: &StdPath) -> Result<Self> {
        // Create parent directory if it doesn't exist
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {:?}", parent))?;
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("Failed to open backend DB at {:?}", db_path))?;

        // forward_addr is the natural primary key: at most one pending request per address
        conn.execute(
            "CREATE TABLE IF NOT EXISTS forwarding_requests (
                forward_addr   TEXT PRIMARY KEY,
                dest_domain    INTEGER NOT NULL,
                dest_recipient TEXT NOT NULL,
                token_id       TEXT NOT NULL,
                created_at     TEXT NOT NULL
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
    /// Returns (request, true) if newly created, (request, false) if existing was returned.
    pub fn create_request(
        &self,
        create_req: CreateForwardingRequest,
    ) -> Result<(ForwardingRequest, bool)> {
        let conn = self.conn.lock().unwrap();

        let created_at = chrono::Utc::now().to_rfc3339();

        conn.execute(
            "INSERT OR IGNORE INTO forwarding_requests (forward_addr, dest_domain, dest_recipient, token_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &create_req.forward_addr,
                &create_req.dest_domain,
                &create_req.dest_recipient,
                &create_req.token_id,
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
                token_id: create_req.token_id,
                created_at,
            }
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT forward_addr, dest_domain, dest_recipient, token_id, created_at
                     FROM forwarding_requests WHERE forward_addr = ?1",
                )
                .context("Failed to prepare SELECT statement")?;

            let existing = stmt
                .query_row(params![&create_req.forward_addr], |row| {
                    Ok(ForwardingRequest {
                        forward_addr: row.get(0)?,
                        dest_domain: row.get(1)?,
                        dest_recipient: row.get(2)?,
                        token_id: row.get(3)?,
                        created_at: row.get(4)?,
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

    /// Add a forwarding request (for testing)
    pub fn add_request(&self, request: ForwardingRequest) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO forwarding_requests (forward_addr, dest_domain, dest_recipient, token_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &request.forward_addr,
                &request.dest_domain,
                &request.dest_recipient,
                &request.token_id,
                &request.created_at
            ],
        )
        .context("Failed to add forwarding request")?;

        Ok(())
    }

    /// Get all forwarding requests
    pub fn list_requests(&self) -> Result<Vec<ForwardingRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT forward_addr, dest_domain, dest_recipient, token_id, created_at
                 FROM forwarding_requests ORDER BY created_at",
            )
            .context("Failed to prepare SELECT statement")?;

        let rows = stmt
            .query_map([], |row| {
                Ok(ForwardingRequest {
                    forward_addr: row.get(0)?,
                    dest_domain: row.get(1)?,
                    dest_recipient: row.get(2)?,
                    token_id: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .context("Failed to query forwarding requests")?;

        let mut requests = Vec::new();
        for row in rows {
            requests.push(row.context("Failed to read row")?);
        }

        Ok(requests)
    }

    pub fn pending_metrics_snapshot(&self) -> Result<PendingRequestMetricsSnapshot> {
        let conn = self.conn.lock().unwrap();

        let pending_requests =
            conn.query_row("SELECT COUNT(*) FROM forwarding_requests", [], |row| {
                row.get::<_, i64>(0)
            })
            .context("Failed to query pending request count")? as usize;

        let oldest_created_at = conn
            .query_row(
                "SELECT MIN(created_at) FROM forwarding_requests",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("Failed to query oldest pending request timestamp")?
            .flatten();

        Ok(PendingRequestMetricsSnapshot {
            pending_requests,
            oldest_created_at,
        })
    }

    /// Remove a request by address (called when forwarding completes)
    pub fn remove_by_addr(&self, forward_addr: &str) -> Result<Option<ForwardingRequest>> {
        let conn = self.conn.lock().unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT forward_addr, dest_domain, dest_recipient, token_id, created_at
                 FROM forwarding_requests WHERE forward_addr = ?1",
            )
            .context("Failed to prepare SELECT statement")?;

        let request = stmt
            .query_row(params![forward_addr], |row| {
                Ok(ForwardingRequest {
                    forward_addr: row.get(0)?,
                    dest_domain: row.get(1)?,
                    dest_recipient: row.get(2)?,
                    token_id: row.get(3)?,
                    created_at: row.get(4)?,
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

/// Backend state (using SQLite storage)
#[derive(Clone)]
pub struct BackendState {
    storage: Arc<BackendStorage>,
    metrics_enabled: bool,
}

impl BackendState {
    pub fn new(db_path: PathBuf, metrics_enabled: bool) -> Result<Self> {
        let storage = BackendStorage::new(&db_path)?;
        let count = storage.list_requests()?.len();
        info!("Loaded {} pending requests from database", count);

        Ok(Self {
            storage: Arc::new(storage),
            metrics_enabled,
        })
    }

    pub fn add_request(&self, request: ForwardingRequest) -> Result<()> {
        self.storage.add_request(request)
    }

    pub fn create_request(
        &self,
        create_req: CreateForwardingRequest,
    ) -> Result<(ForwardingRequest, bool)> {
        self.storage.create_request(create_req)
    }

    pub fn list_requests(&self) -> Result<Vec<ForwardingRequest>> {
        self.storage.list_requests()
    }

    pub fn remove_by_addr(&self, forward_addr: &str) -> Result<Option<ForwardingRequest>> {
        self.storage.remove_by_addr(forward_addr)
    }

    pub fn metrics_enabled(&self) -> bool {
        self.metrics_enabled
    }

    pub fn refresh_metrics(&self) -> Result<()> {
        if !self.metrics_enabled {
            return Ok(());
        }

        let snapshot = self.storage.pending_metrics_snapshot()?;
        update_pending_request_metrics(&snapshot)?;
        Ok(())
    }
}

/// Backend server
pub struct Backend {
    state: BackendState,
    port: u16,
}

impl Backend {
    pub fn new(port: u16, db_path: PathBuf, metrics_enabled: bool) -> Result<Self> {
        Ok(Self {
            state: BackendState::new(db_path, metrics_enabled)?,
            port,
        })
    }

    pub fn state(&self) -> BackendState {
        self.state.clone()
    }

    /// Start the backend server
    pub async fn serve(self) -> Result<()> {
        if self.state.metrics_enabled() {
            self.state.refresh_metrics()?;

            let state = self.state.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(BACKEND_METRICS_REFRESH_INTERVAL).await;

                    if let Err(err) = state.refresh_metrics() {
                        error!("Failed to refresh backend metrics: {err:#}");
                    }
                }
            });
        }

        let app = Router::new()
            .route("/forwarding-address", get(get_forwarding_address))
            .route(
                "/forwarding-requests",
                get(list_requests).post(create_request),
            )
            .route("/forwarding-requests/:addr", delete(complete_request))
            .with_state(self.state);

        let addr = format!("0.0.0.0:{}", self.port);
        info!("Backend listening on {}", addr);

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

pub fn oldest_pending_request_age_seconds(created_at: Option<&str>) -> Result<f64> {
    let Some(created_at) = created_at else {
        return Ok(0.0);
    };

    let created_at = chrono::DateTime::parse_from_rfc3339(created_at)
        .with_context(|| format!("Invalid forwarding request timestamp: {created_at}"))?;
    let age = chrono::Utc::now()
        .signed_duration_since(created_at.with_timezone(&chrono::Utc))
        .num_seconds();

    Ok(age.max(0) as f64)
}

/// Query params for GET /forwarding-address
#[derive(Deserialize)]
struct ForwardingAddressQuery {
    dest_domain: u32,
    dest_recipient: String,
    token_id: String,
}

/// GET /forwarding-address?dest_domain=<u32>&dest_recipient=<hex>&token_id=<hex> - Derive forwarding address
async fn get_forwarding_address(Query(params): Query<ForwardingAddressQuery>) -> impl IntoResponse {
    match derive_forwarding_address(params.dest_domain, &params.dest_recipient, &params.token_id) {
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

/// GET /forwarding-requests - List all forwarding requests
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

/// POST /forwarding-requests - Create a new forwarding request, or return the existing one for
/// the address (idempotent). Returns 201 if newly created, 200 if existing was returned.
async fn create_request(
    State(state): State<BackendState>,
    Json(create_req): Json<CreateForwardingRequest>,
) -> impl IntoResponse {
    match derive_forwarding_address(
        create_req.dest_domain,
        &create_req.dest_recipient,
        &create_req.token_id,
    ) {
        Ok(derived) if derived == create_req.forward_addr => {}
        Ok(derived) => {
            if state.metrics_enabled() {
                counter!("requests_created_total", "result" => "address_mismatch").increment(1);
            }
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "forward_addr does not match derivation from dest_domain/dest_recipient/token_id",
                    "expected": derived,
                    "got": create_req.forward_addr,
                })),
            )
                .into_response();
        }
        Err(e) => {
            if state.metrics_enabled() {
                counter!("requests_created_total", "result" => "invalid_params").increment(1);
            }
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    match state.create_request(create_req) {
        Ok((request, created)) => {
            if state.metrics_enabled() {
                if created {
                    counter!("requests_created_total", "result" => "created").increment(1);
                } else {
                    counter!("requests_created_total", "result" => "existing").increment(1);
                }

                if let Err(err) = state.refresh_metrics() {
                    error!("Failed to refresh backend metrics after create: {err:#}");
                }
            }

            let status_code = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (status_code, Json(request)).into_response()
        }
        Err(e) => {
            if state.metrics_enabled() {
                counter!("requests_created_total", "result" => "error").increment(1);
            }
            error!("Failed to create forwarding request: {:#}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// DELETE /forwarding-requests/:addr - Mark forwarding as complete by removing the request
async fn complete_request(
    Path(addr): Path<String>,
    State(state): State<BackendState>,
) -> impl IntoResponse {
    match state.remove_by_addr(&addr) {
        Ok(Some(request)) => {
            if state.metrics_enabled() {
                counter!("requests_completed_total", "result" => "removed").increment(1);
                if let Err(err) = state.refresh_metrics() {
                    error!("Failed to refresh backend metrics after delete: {err:#}");
                }
            }
            info!("Removed completed request for address {}", addr);
            (StatusCode::OK, Json(request)).into_response()
        }
        Ok(None) => {
            if state.metrics_enabled() {
                counter!("requests_completed_total", "result" => "not_found").increment(1);
            }
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            if state.metrics_enabled() {
                counter!("requests_completed_total", "result" => "error").increment(1);
            }
            error!("Failed to remove request for {}: {:#}", addr, e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

fn update_pending_request_metrics(snapshot: &PendingRequestMetricsSnapshot) -> Result<()> {
    gauge!("pending_requests").set(snapshot.pending_requests as f64);

    let age_seconds = oldest_pending_request_age_seconds(snapshot.oldest_created_at.as_deref())?;
    gauge!("oldest_pending_request_age_seconds").set(age_seconds);

    Ok(())
}
