use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch},
    Json, Router,
};
use clap::Parser;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{error, info};

use crate::{CreateForwardingRequest, ForwardingRequest, StatusUpdate};

/// Backend configuration
#[derive(Parser, Debug)]
pub struct BackendConfig {
    /// Port to listen on
    #[arg(long, env = "PORT", default_value = "8080")]
    pub port: u16,

    /// Path to database file
    #[arg(long, env = "DB_PATH", default_value = "storage/backend.db")]
    pub db_path: PathBuf,
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

        // Create tables if they don't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS forwarding_requests (
                id TEXT PRIMARY KEY,
                forward_addr TEXT NOT NULL,
                dest_domain INTEGER NOT NULL,
                dest_recipient TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT
            )",
            [],
        )
        .context("Failed to create forwarding_requests table")?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS backend_metadata (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            )",
            [],
        )
        .context("Failed to create backend_metadata table")?;

        info!("Opened backend database at {:?}", db_path);

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Increment and return the next ID
    pub fn increment_next_id(&self) -> Result<u64> {
        let conn = self.conn.lock().unwrap();
        let current = {
            let mut stmt = conn
                .prepare("SELECT value FROM backend_metadata WHERE key = 'next_id'")
                .context("Failed to prepare SELECT statement")?;
            let result: Option<u64> = stmt
                .query_row([], |row| row.get(0))
                .optional()
                .context("Failed to query next_id")?;
            result.unwrap_or(1)
        };

        conn.execute(
            "INSERT OR REPLACE INTO backend_metadata (key, value) VALUES ('next_id', ?1)",
            params![current + 1],
        )
        .context("Failed to update next_id")?;

        Ok(current)
    }

    /// Create a new forwarding request
    pub fn create_request(&self, create_req: CreateForwardingRequest) -> Result<ForwardingRequest> {
        let id_num = self.increment_next_id()?;
        let id = format!("req-{:06}", id_num);

        let request = ForwardingRequest {
            id: id.clone(),
            forward_addr: create_req.forward_addr,
            dest_domain: create_req.dest_domain,
            dest_recipient: create_req.dest_recipient,
            status: "pending".to_string(),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
        };

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO forwarding_requests (id, forward_addr, dest_domain, dest_recipient, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &request.id,
                &request.forward_addr,
                &request.dest_domain,
                &request.dest_recipient,
                &request.status,
                &request.created_at
            ],
        )
        .context("Failed to insert forwarding request")?;

        info!("Created forwarding request: {}", id);
        Ok(request)
    }

    /// Add a forwarding request (for testing)
    pub fn add_request(&self, request: ForwardingRequest) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO forwarding_requests (id, forward_addr, dest_domain, dest_recipient, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &request.id,
                &request.forward_addr,
                &request.dest_domain,
                &request.dest_recipient,
                &request.status,
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
            .prepare("SELECT id, forward_addr, dest_domain, dest_recipient, status, created_at FROM forwarding_requests ORDER BY created_at")
            .context("Failed to prepare SELECT statement")?;

        let rows = stmt
            .query_map([], |row| {
                Ok(ForwardingRequest {
                    id: row.get(0)?,
                    forward_addr: row.get(1)?,
                    dest_domain: row.get(2)?,
                    dest_recipient: row.get(3)?,
                    status: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .context("Failed to query forwarding requests")?;

        let mut requests = Vec::new();
        for row in rows {
            requests.push(row.context("Failed to read row")?);
        }

        Ok(requests)
    }

    /// Get a specific request by ID
    pub fn get_request(&self, id: &str) -> Result<Option<ForwardingRequest>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, forward_addr, dest_domain, dest_recipient, status, created_at FROM forwarding_requests WHERE id = ?1")
            .context("Failed to prepare SELECT statement")?;

        let result = stmt
            .query_row(params![id], |row| {
                Ok(ForwardingRequest {
                    id: row.get(0)?,
                    forward_addr: row.get(1)?,
                    dest_domain: row.get(2)?,
                    dest_recipient: row.get(3)?,
                    status: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .optional()
            .context("Failed to query forwarding request")?;

        Ok(result)
    }

    /// Update request status
    pub fn update_status(&self, id: &str, status: &str) -> Result<Option<ForwardingRequest>> {
        let conn = self.conn.lock().unwrap();

        // Check if request exists
        let mut stmt = conn
            .prepare("SELECT id, forward_addr, dest_domain, dest_recipient, status, created_at FROM forwarding_requests WHERE id = ?1")
            .context("Failed to prepare SELECT statement")?;

        let request = stmt
            .query_row(params![id], |row| {
                Ok(ForwardingRequest {
                    id: row.get(0)?,
                    forward_addr: row.get(1)?,
                    dest_domain: row.get(2)?,
                    dest_recipient: row.get(3)?,
                    status: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .optional()
            .context("Failed to query forwarding request")?;

        if let Some(mut req) = request {
            conn.execute(
                "UPDATE forwarding_requests SET status = ?1 WHERE id = ?2",
                params![status, id],
            )
            .context("Failed to update request status")?;

            req.status = status.to_string();
            Ok(Some(req))
        } else {
            Ok(None)
        }
    }

    /// Remove a request (for completed requests)
    pub fn remove_request(&self, id: &str) -> Result<Option<ForwardingRequest>> {
        let conn = self.conn.lock().unwrap();

        // Get the request before deleting
        let mut stmt = conn
            .prepare("SELECT id, forward_addr, dest_domain, dest_recipient, status, created_at FROM forwarding_requests WHERE id = ?1")
            .context("Failed to prepare SELECT statement")?;

        let request = stmt
            .query_row(params![id], |row| {
                Ok(ForwardingRequest {
                    id: row.get(0)?,
                    forward_addr: row.get(1)?,
                    dest_domain: row.get(2)?,
                    dest_recipient: row.get(3)?,
                    status: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .optional()
            .context("Failed to query forwarding request")?;

        if request.is_some() {
            conn.execute("DELETE FROM forwarding_requests WHERE id = ?1", params![id])
                .context("Failed to delete forwarding request")?;
        }

        Ok(request)
    }
}

/// Backend state (using SQLite storage)
#[derive(Clone)]
pub struct BackendState {
    storage: Arc<BackendStorage>,
}

impl BackendState {
    pub fn new(db_path: PathBuf) -> Result<Self> {
        let storage = BackendStorage::new(&db_path)?;
        let count = storage.list_requests()?.len();
        info!("Loaded {} pending requests from database", count);

        Ok(Self {
            storage: Arc::new(storage),
        })
    }

    pub fn add_request(&self, request: ForwardingRequest) -> Result<()> {
        self.storage.add_request(request)
    }

    pub fn create_request(&self, create_req: CreateForwardingRequest) -> Result<ForwardingRequest> {
        self.storage.create_request(create_req)
    }

    pub fn list_requests(&self) -> Result<Vec<ForwardingRequest>> {
        self.storage.list_requests()
    }

    pub fn get_request(&self, id: &str) -> Result<Option<ForwardingRequest>> {
        self.storage.get_request(id)
    }

    pub fn update_status(&self, id: &str, status: &str) -> Result<Option<ForwardingRequest>> {
        self.storage.update_status(id, status)
    }

    pub fn remove_request(&self, id: &str) -> Result<Option<ForwardingRequest>> {
        self.storage.remove_request(id)
    }
}

/// Backend server
pub struct Backend {
    state: BackendState,
    port: u16,
}

impl Backend {
    pub fn new(port: u16, db_path: PathBuf) -> Result<Self> {
        Ok(Self {
            state: BackendState::new(db_path)?,
            port,
        })
    }

    pub fn state(&self) -> BackendState {
        self.state.clone()
    }

    /// Start the backend server
    pub async fn serve(self) -> Result<()> {
        let app = Router::new()
            .route(
                "/forwarding-requests",
                get(list_requests).post(create_request),
            )
            .route(
                "/forwarding-requests/:id/status",
                patch(update_request_status),
            )
            .with_state(self.state);

        let addr = format!("127.0.0.1:{}", self.port);
        info!("Backend listening on {}", addr);

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }
}

// Legacy type aliases for backward compatibility
pub type MockBackend = Backend;
pub type MockBackendState = BackendState;
pub type MockBackendConfig = BackendConfig;

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

/// POST /forwarding-requests - Create a new forwarding request with auto-generated ID
async fn create_request(
    State(state): State<BackendState>,
    Json(create_req): Json<CreateForwardingRequest>,
) -> impl IntoResponse {
    match state.create_request(create_req) {
        Ok(request) => {
            info!(
                "Created forwarding request {} for address {}",
                request.id, request.forward_addr
            );
            (StatusCode::CREATED, Json(request)).into_response()
        }
        Err(e) => {
            error!("Failed to create forwarding request: {:#}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ForwardingRequest {
                    id: String::new(),
                    forward_addr: String::new(),
                    dest_domain: 0,
                    dest_recipient: String::new(),
                    status: "error".to_string(),
                    created_at: None,
                }),
            )
                .into_response()
        }
    }
}

/// PATCH /forwarding-requests/{id}/status - Update forwarding request status
/// When status is "completed", the request is removed from storage
async fn update_request_status(
    Path(id): Path<String>,
    State(state): State<BackendState>,
    Json(update): Json<StatusUpdate>,
) -> impl IntoResponse {
    // If status is "completed", remove from storage
    if update.status == "completed" {
        match state.remove_request(&id) {
            Ok(Some(request)) => {
                info!(
                    "Removed completed request {} (address {}) from storage",
                    id, request.forward_addr
                );
                return (StatusCode::OK, Json(request)).into_response();
            }
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(ForwardingRequest {
                        id: String::new(),
                        forward_addr: String::new(),
                        dest_domain: 0,
                        dest_recipient: String::new(),
                        status: "error".to_string(),
                        created_at: None,
                    }),
                )
                    .into_response();
            }
            Err(e) => {
                error!("Failed to remove request {}: {:#}", id, e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ForwardingRequest {
                        id: String::new(),
                        forward_addr: String::new(),
                        dest_domain: 0,
                        dest_recipient: String::new(),
                        status: "error".to_string(),
                        created_at: None,
                    }),
                )
                    .into_response();
            }
        }
    }

    // For other status updates, just update in-place
    match state.update_status(&id, &update.status) {
        Ok(Some(request)) => {
            info!(
                "Updated status for request {} (address {}): {}",
                id, request.forward_addr, update.status
            );
            (StatusCode::OK, Json(request)).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ForwardingRequest {
                id: String::new(),
                forward_addr: String::new(),
                dest_domain: 0,
                dest_recipient: String::new(),
                status: "error".to_string(),
                created_at: None,
            }),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to update request {}: {:#}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ForwardingRequest {
                    id: String::new(),
                    forward_addr: String::new(),
                    dest_domain: 0,
                    dest_recipient: String::new(),
                    status: "error".to_string(),
                    created_at: None,
                }),
            )
                .into_response()
        }
    }
}
