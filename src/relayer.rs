use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use metrics::{counter, gauge};
use reqwest::Client as HttpClient;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use crate::client::CelestiaClient;
use crate::scanner;
use crate::{Balance, ForwardingRequest};

/// Per-request retry backoff schedule (seconds) applied after a failed submission.
/// After exhausting the schedule, the final value (1 hour) is reused for every
/// subsequent attempt until the request is dropped for exceeding its max age.
const RETRY_BACKOFF_SCHEDULE_SECONDS: [u64; 5] = [30, 60, 300, 1800, 3600];

/// Returns the delay to wait before the next submission attempt, given how many
/// submissions have already failed for a request (`failures` >= 1). The delay
/// follows [`RETRY_BACKOFF_SCHEDULE_SECONDS`] and saturates at its last entry
/// (1 hour), independent of the configured max request age.
pub fn retry_delay(failures: u32) -> Duration {
    let idx = (failures.saturating_sub(1) as usize).min(RETRY_BACKOFF_SCHEDULE_SECONDS.len() - 1);
    Duration::from_secs(RETRY_BACKOFF_SCHEDULE_SECONDS[idx])
}

/// Tracks submission backoff for a single forwarding request.
struct RetryState {
    /// Number of failed submission attempts so far.
    failures: u32,
    /// Wall-clock time before which the next submission attempt must not happen.
    /// Wall-clock (not a monotonic `Instant`) so it can be persisted and is
    /// still meaningful after a relayer restart.
    next_attempt: DateTime<Utc>,
}

/// SQLite-backed persistence for per-request submission backoff, address activity,
/// and the block-scan height cursor, so progress survives a relayer crash or restart
/// instead of resetting. A single connection shared (behind a mutex) across the
/// scanner, maintenance, and forward-worker tasks.
pub(crate) struct RetryStore {
    conn: Connection,
}

impl RetryStore {
    /// Open (creating if needed) the retry-state database.
    fn new(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create directory {:?}", parent))?;
            }
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("Failed to open retry-state DB at {:?}", db_path))?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS retry_state (
                forward_addr    TEXT PRIMARY KEY,
                failures        INTEGER NOT NULL,
                next_attempt_at TEXT NOT NULL
            )",
            [],
        )
        .context("Failed to create retry_state table")?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS address_activity (
                forward_addr     TEXT PRIMARY KEY,
                last_activity_at TEXT NOT NULL
            )",
            [],
        )
        .context("Failed to create address_activity table")?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS scan_state (
                id     INTEGER PRIMARY KEY CHECK (id = 0),
                height INTEGER NOT NULL
            )",
            [],
        )
        .context("Failed to create scan_state table")?;

        info!("Opened relayer retry-state database at {:?}", db_path);

        Ok(Self { conn })
    }

    /// Load the last fully-scanned block height, if any.
    pub(crate) fn load_height(&self) -> Result<Option<u64>> {
        let height: Option<i64> = self
            .conn
            .query_row("SELECT height FROM scan_state WHERE id = 0", [], |row| {
                row.get(0)
            })
            .optional()
            .context("Failed to query scan height")?;
        Ok(height.map(|h| h.max(0) as u64))
    }

    /// Persist the last fully-scanned block height.
    pub(crate) fn store_height(&self, height: u64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO scan_state (id, height) VALUES (0, ?1)",
                params![height as i64],
            )
            .context("Failed to persist scan height")?;
        Ok(())
    }

    /// Load all persisted backoff state, skipping rows that fail to parse.
    fn load(&self) -> Result<HashMap<String, RetryState>> {
        let mut stmt = self
            .conn
            .prepare("SELECT forward_addr, failures, next_attempt_at FROM retry_state")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        let mut map = HashMap::new();
        for row in rows {
            let (addr, failures, next_attempt_at) = row?;
            match DateTime::parse_from_rfc3339(&next_attempt_at) {
                Ok(ts) => {
                    map.insert(
                        addr,
                        RetryState {
                            failures: failures.max(0) as u32,
                            next_attempt: ts.with_timezone(&Utc),
                        },
                    );
                }
                Err(e) => warn!("Skipping unparseable retry-state row for {}: {}", addr, e),
            }
        }
        Ok(map)
    }

    /// Insert or update the backoff state for an address.
    fn upsert(&self, addr: &str, state: &RetryState) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO retry_state (forward_addr, failures, next_attempt_at)
                 VALUES (?1, ?2, ?3)",
                params![addr, state.failures as i64, state.next_attempt.to_rfc3339()],
            )
            .with_context(|| format!("Failed to persist retry state for {}", addr))?;
        Ok(())
    }

    /// Delete the backoff state for an address (e.g. on success or drop).
    fn remove(&self, addr: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM retry_state WHERE forward_addr = ?1",
                params![addr],
            )
            .with_context(|| format!("Failed to delete retry state for {}", addr))?;
        Ok(())
    }

    /// Load all persisted activity timestamps, skipping rows that fail to parse.
    fn load_activity(&self) -> Result<HashMap<String, DateTime<Utc>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT forward_addr, last_activity_at FROM address_activity")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut map = HashMap::new();
        for row in rows {
            let (addr, last_activity_at) = row?;
            match DateTime::parse_from_rfc3339(&last_activity_at) {
                Ok(ts) => {
                    map.insert(addr, ts.with_timezone(&Utc));
                }
                Err(e) => warn!("Skipping unparseable activity row for {}: {}", addr, e),
            }
        }
        Ok(map)
    }

    /// Insert or update the last-activity timestamp for an address.
    fn upsert_activity(&self, addr: &str, ts: &DateTime<Utc>) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO address_activity (forward_addr, last_activity_at)
                 VALUES (?1, ?2)",
                params![addr, ts.to_rfc3339()],
            )
            .with_context(|| format!("Failed to persist activity for {}", addr))?;
        Ok(())
    }

    /// Delete the activity timestamp for an address (e.g. on retirement).
    fn remove_activity(&self, addr: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM address_activity WHERE forward_addr = ?1",
                params![addr],
            )
            .with_context(|| format!("Failed to delete activity for {}", addr))?;
        Ok(())
    }

    /// Delete persisted state for any address not in `active` (no longer pending),
    /// across both the retry-state and address-activity tables.
    fn retain(&self, active: &HashSet<&str>) -> Result<()> {
        for table in ["retry_state", "address_activity"] {
            let mut stmt = self
                .conn
                .prepare(&format!("SELECT forward_addr FROM {table}"))?;
            let addrs: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            for addr in addrs {
                if !active.contains(addr.as_str()) {
                    self.conn.execute(
                        &format!("DELETE FROM {table} WHERE forward_addr = ?1"),
                        params![addr],
                    )?;
                }
            }
        }
        Ok(())
    }
}

/// Compare two balance lists for equality (order-independent).
pub fn balances_equal(a: &[Balance], b: &[Balance]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<_> = a.iter().map(|c| (&c.denom, &c.amount)).collect();
    let mut b: Vec<_> = b.iter().map(|c| (&c.denom, &c.amount)).collect();
    a.sort_by(|x, y| x.0.cmp(y.0));
    b.sort_by(|x, y| x.0.cmp(y.0));
    a == b
}

/// Relayer configuration
#[derive(Parser, Debug)]
pub struct RelayerConfig {
    /// Celestia gRPC URL (port 9090). Used for balance queries, IGP fee quotes,
    /// and transaction submission.
    ///
    /// Accepts a comma-separated list for redundancy, e.g.
    /// `http://node-a:9090,http://node-b:9090`. The first URL is the primary and
    /// the rest are fallbacks: queries fail over to the next endpoint within the
    /// same call, and a failed transaction submission rotates the preferred
    /// endpoint so the backoff retry runs against the fallback.
    #[arg(long, env = "CELESTIA_GRPC", default_value = "http://localhost:9090")]
    pub celestia_grpc: String,

    /// Celestia CometBFT RPC URL (port 26657). Used to scan committed blocks for
    /// deposit events and to subscribe to new blocks over WebSocket. The WebSocket
    /// URL is derived from this (http→ws, https→wss, plus `/websocket`).
    ///
    /// Accepts a comma-separated list for redundancy, e.g.
    /// `http://node-a:26657,http://node-b:26657`. The first URL is the primary and
    /// the rest are fallbacks: the scanner runs against one endpoint at a time and
    /// rotates to the next whenever a session fails.
    #[arg(long, env = "CELESTIA_RPC", default_value = "http://localhost:26657")]
    pub celestia_rpc: String,

    /// Backend API URL
    #[arg(long, env = "BACKEND_URL", default_value = "http://localhost:8080")]
    pub backend_url: String,

    /// Relayer secp256k1 private key hex (for signing transactions)
    #[arg(long, env = "PRIVATE_KEY_HEX")]
    pub private_key_hex: String,

    /// Optional custom IGP hook id (hex) to route each forward's interchain gas
    /// payment through, e.g. an alternative IGP this relayer watches. When set, it
    /// is passed as MsgForward.custom_hook_id so the fee is paid to that IGP (and
    /// this relayer, rather than the mailbox default hook / default relayer).
    /// Empty/unset => mailbox default hook (unchanged behavior).
    #[arg(long, env = "CUSTOM_IGP_HOOK")]
    pub custom_igp_hook: Option<String>,

    /// Signer-balance metrics refresh interval in seconds.
    #[arg(long, env = "POLL_INTERVAL", default_value = "6")]
    pub poll_interval: u64,

    /// Interval in seconds between maintenance ticks (live-list refresh from the
    /// backend, retention sweep, and retry re-enqueue).
    #[arg(long, env = "MAINTENANCE_INTERVAL", default_value = "30")]
    pub maintenance_interval: u64,

    /// Interval in seconds between balance-poll backstop sweeps over the live list.
    /// This is the emergency fallback, not the primary fix: `block_confirmation_depth`
    /// already keeps the scanner far enough behind the tip that it never reads a
    /// not-yet-indexed block, so in normal operation this sweep should never be the
    /// thing that catches a deposit. It exists only as defense-in-depth against an
    /// unforeseen scanner gap — a periodic re-query of every live address that catches
    /// any missed deposit on the next sweep, independent of the exact block. Set to 0
    /// to disable.
    #[arg(long, env = "BALANCE_POLL_INTERVAL", default_value = "3600")]
    pub balance_poll_interval: u64,

    /// Maximum number of forwarding submissions processed concurrently.
    #[arg(long, env = "FORWARD_CONCURRENCY", default_value = "64")]
    pub forward_concurrency: usize,

    /// Block height to begin scanning from on first run (no persisted cursor).
    /// Defaults to the chain tip at startup when unset.
    #[arg(long, env = "BLOCK_SCAN_START_HEIGHT")]
    pub block_scan_start_height: Option<u64>,

    /// Number of blocks to lag behind the chain tip before scanning a height. This
    /// is NOT a reorg-safety depth — CometBFT commits are absolutely final. It only
    /// absorbs the brief read-after-write window where a `NewBlock` notification can
    /// outrace the same node's RPC tx-result indexing and return an empty
    /// `block_results` for the just-committed tip. One block of block-time is enough;
    /// the default of 2 adds margin. With this set, the balance-poll backstop should
    /// effectively never be the thing that catches a deposit. Set to 0 to scan the
    /// tip immediately (the old behavior).
    #[arg(long, env = "BLOCK_CONFIRMATION_DEPTH", default_value = "2")]
    pub block_confirmation_depth: u64,

    /// IGP fee buffer multiplier (e.g., 1.1 for 10% buffer)
    #[arg(long, env = "IGP_FEE_BUFFER", default_value = "1.1")]
    pub igp_fee_buffer: f64,

    /// Maximum age for a forwarding request in seconds before it's considered dead (default: 86400 = 1 day)
    #[arg(long, env = "MAX_REQUEST_AGE_SECONDS", default_value = "86400")]
    pub max_request_age_seconds: u64,

    /// Max seconds an active address (one that has seen a deposit or forward) may
    /// go unused before it's removed from the live monitoring list (default:
    /// 604800 = 7 days). Distinct from `max_request_age_seconds`, which only
    /// governs addresses that have never seen any activity.
    #[arg(long, env = "MAX_ADDRESS_INACTIVITY_SECONDS", default_value = "604800")]
    pub max_address_inactivity_seconds: u64,

    /// Path to the relayer's retry-state database, which persists submission
    /// backoff across restarts so a crash does not reset every request to an
    /// immediate re-attempt
    #[arg(
        long,
        env = "RETRY_STATE_DB_PATH",
        default_value = "storage/relayer-retry.db"
    )]
    pub retry_state_db_path: PathBuf,

    /// Metrics port for Prometheus scraping
    #[arg(long, env = "RELAYER_METRICS_PORT")]
    pub metrics_port: Option<u16>,
}

/// Time-to-live for cached IGP fee quotes (shared across all addresses with the
/// same destination, so a busy domain isn't re-quoted on every forward).
const FEE_CACHE_TTL: Duration = Duration::from_secs(60);

/// Capacity of the forward-trigger channel. Generous (entries are ~tiny address
/// strings) but bounded, so a producer burst applies backpressure rather than
/// growing memory without limit.
const FORWARD_QUEUE_CAPACITY: usize = 10_000;

/// IGP fee quote cache: (dest_domain, token_id) -> (quoted fee, fetched-at).
type FeeCache = Arc<Mutex<HashMap<(u32, String), (f64, Instant)>>>;

/// Cheaply-cloneable handles to all relayer state, shared across the scanner,
/// maintenance, and forward-worker tasks. Each `Mutex` guards a short, non-async
/// critical section only — locks are never held across an `.await`.
#[derive(Clone)]
struct RelayerState {
    config: Arc<RelayerConfig>,
    celestia: Arc<CelestiaClient>,
    http_client: HttpClient,
    /// Known forwarding addresses (the live list), synced from the backend. The
    /// scanner checks membership against this to decide which deposits to act on.
    live: Arc<Mutex<HashMap<String, ForwardingRequest>>>,
    /// Per-address submission backoff for addresses awaiting a successful forward.
    /// Mirrored to `store` so it survives restarts.
    retry_state: Arc<Mutex<HashMap<String, RetryState>>>,
    /// Last activity (deposit observed or forward succeeded) per address. Absence
    /// means never active (on the `max_request_age_seconds` timer); presence means
    /// active (on the `max_address_inactivity_seconds` timer). Mirrored to `store`.
    last_activity: Arc<Mutex<HashMap<String, DateTime<Utc>>>>,
    /// IGP fee quote cache keyed by (dest_domain, token_id), with a short TTL.
    fee_cache: FeeCache,
    /// Durable store (single connection, mutex-guarded) for retry, activity, and
    /// the block-scan height cursor.
    store: Arc<Mutex<RetryStore>>,
}

impl RelayerState {
    /// Record activity (a deposit or successful forward) for an address now,
    /// updating both the in-memory cache and the durable store.
    fn record_activity(&self, forward_addr: &str) {
        let now = Utc::now();
        if let Err(e) = self
            .store
            .lock()
            .unwrap()
            .upsert_activity(forward_addr, &now)
        {
            warn!("Failed to persist activity for {}: {:#}", forward_addr, e);
        }
        self.last_activity
            .lock()
            .unwrap()
            .insert(forward_addr.to_string(), now);
    }

    /// Clear any backoff state for an address from both the in-memory cache and the
    /// durable store (on successful/again-empty forward or when retiring the request).
    fn clear_retry_state(&self, forward_addr: &str) {
        self.retry_state.lock().unwrap().remove(forward_addr);
        if let Err(e) = self.store.lock().unwrap().remove(forward_addr) {
            warn!(
                "Failed to clear persisted retry state for {}: {:#}",
                forward_addr, e
            );
        }
    }

    /// Clear the activity timestamp for an address (when retiring the request).
    fn clear_activity(&self, forward_addr: &str) {
        self.last_activity.lock().unwrap().remove(forward_addr);
        if let Err(e) = self.store.lock().unwrap().remove_activity(forward_addr) {
            warn!(
                "Failed to clear persisted activity for {}: {:#}",
                forward_addr, e
            );
        }
    }

    /// Advance exponential backoff after any failed forward attempt (balance query,
    /// fee query, or submission), persisting the new state so the maintenance
    /// retry-due sweep re-enqueues the address once the delay elapses. Returns the
    /// scheduled delay for logging.
    fn note_failure(&self, forward_addr: &str) -> Duration {
        let failures = self
            .retry_state
            .lock()
            .unwrap()
            .get(forward_addr)
            .map_or(0, |s| s.failures)
            + 1;
        let delay = retry_delay(failures);
        let next_attempt = Utc::now()
            + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::seconds(3600));
        let state = RetryState {
            failures,
            next_attempt,
        };
        if let Err(e) = self.store.lock().unwrap().upsert(forward_addr, &state) {
            warn!(
                "Failed to persist retry state for {}: {:#}",
                forward_addr, e
            );
        }
        self.retry_state
            .lock()
            .unwrap()
            .insert(forward_addr.to_string(), state);
        delay
    }

    /// Fetch the live list of forwarding requests from the backend.
    async fn fetch_forwarding_requests(&self) -> Result<Vec<ForwardingRequest>> {
        let url = format!("{}/forwarding-requests", self.config.backend_url);
        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch forwarding requests from backend")?;

        if !response.status().is_success() {
            anyhow::bail!(
                "Backend returned error: {} - {}",
                response.status(),
                response.text().await.unwrap_or_default()
            );
        }

        response
            .json::<Vec<ForwardingRequest>>()
            .await
            .context("Failed to parse forwarding requests")
    }

    /// Notify the backend that an address is done (removes it from the registry).
    async fn complete_request(&self, forward_addr: &str) -> Result<()> {
        let url = format!(
            "{}/forwarding-requests/{}",
            self.config.backend_url, forward_addr
        );
        let response = self
            .http_client
            .delete(&url)
            .send()
            .await
            .with_context(|| format!("Failed to complete request for {}", forward_addr))?;

        if !response.status().is_success() {
            warn!(
                "Failed to remove backend request for {}: {}",
                forward_addr,
                response.status()
            );
        } else {
            info!("Removed completed request for address {}", forward_addr);
        }
        Ok(())
    }
}

/// Relayer entry point. Owns the shared state and spawns the worker tasks.
pub struct Relayer {
    shared: RelayerState,
}

impl Relayer {
    pub async fn new(config: RelayerConfig) -> Result<Self> {
        let celestia =
            CelestiaClient::new(config.celestia_grpc.clone(), config.private_key_hex.clone())
                .await?;

        info!("Relayer address: {}", celestia.signer_address());

        let http_client = HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;

        let store = RetryStore::new(&config.retry_state_db_path)?;
        let retry_state = store.load().unwrap_or_else(|e| {
            warn!(
                "Failed to load persisted retry state, starting fresh: {:#}",
                e
            );
            HashMap::new()
        });
        info!("Loaded {} persisted retry-state entries", retry_state.len());

        let last_activity = store.load_activity().unwrap_or_else(|e| {
            warn!(
                "Failed to load persisted address activity, starting fresh: {:#}",
                e
            );
            HashMap::new()
        });
        info!(
            "Loaded {} persisted address-activity entries",
            last_activity.len()
        );

        Ok(Self {
            shared: RelayerState {
                config: Arc::new(config),
                celestia: Arc::new(celestia),
                http_client,
                live: Arc::new(Mutex::new(HashMap::new())),
                retry_state: Arc::new(Mutex::new(retry_state)),
                last_activity: Arc::new(Mutex::new(last_activity)),
                fee_cache: Arc::new(Mutex::new(HashMap::new())),
                store: Arc::new(Mutex::new(store)),
            },
        })
    }

    /// Run the relayer: spawn the block scanner, the maintenance ticker, and the
    /// signer-balance metrics loop, then drive the forward-worker dispatcher.
    pub async fn run(self) -> Result<()> {
        let shared = self.shared;
        info!("Starting forwarding relayer (event-driven)");
        info!(
            "Celestia gRPC: {} endpoint(s): {}",
            shared.celestia.endpoint_count(),
            shared.celestia.url_list()
        );
        info!("Celestia RPC:  {}", shared.config.celestia_rpc);
        info!("Backend URL:   {}", shared.config.backend_url);

        // Channel of addresses to (re)attempt a forward for: fed by the scanner
        // (detected deposits), the maintenance ticker (initial probes, retry-due),
        // and consumed by the bounded-concurrency dispatcher. Bounded so a producer
        // burst (e.g. a large restart catch-up) applies backpressure instead of
        // buffering without limit; producers block (batch) once it is full.
        let (deposits_tx, deposits_rx) = mpsc::channel::<String>(FORWARD_QUEUE_CAPACITY);

        // Signer-balance metrics loop.
        {
            let shared = shared.clone();
            tokio::spawn(async move {
                let interval = Duration::from_secs(shared.config.poll_interval.max(1));
                loop {
                    if let Err(e) = refresh_signer_balance_metrics(&shared).await {
                        warn!("Failed to refresh signer balance metrics: {:#}", e);
                    }
                    tokio::time::sleep(interval).await;
                }
            });
        }

        // Maintenance ticker (live-list refresh, retention sweep, retry re-enqueue).
        {
            let shared = shared.clone();
            let tx = deposits_tx.clone();
            tokio::spawn(run_maintenance(shared, tx));
        }

        // Balance-poll backstop: periodically re-checks every live address so a
        // deposit missed by the event-driven scanner (e.g. a node tip read-after-
        // NewBlock race that returns an empty block_results) is still caught on the
        // next sweep instead of being stranded forever.
        if shared.config.balance_poll_interval > 0 {
            let shared = shared.clone();
            let tx = deposits_tx.clone();
            tokio::spawn(run_balance_poll(shared, tx));
        } else {
            warn!("Balance-poll backstop disabled (BALANCE_POLL_INTERVAL=0)");
        }

        // Block scanner (deposit detection).
        {
            let shared = shared.clone();
            let tx = deposits_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = scanner::run_block_scanner(
                    shared.config.celestia_rpc.clone(),
                    shared.config.block_scan_start_height,
                    shared.config.block_confirmation_depth,
                    shared.live.clone(),
                    shared.store.clone(),
                    tx,
                )
                .await
                {
                    error!("Block scanner exited: {:#}", e);
                }
            });
        }

        // Forward-worker dispatcher (runs on this task to keep the process alive).
        run_dispatcher(shared, deposits_tx, deposits_rx).await;
        Ok(())
    }
}

/// Consume triggered addresses and run forwards with bounded concurrency,
/// deduplicating addresses already in flight so the same balance isn't
/// double-submitted concurrently.
async fn run_dispatcher(
    shared: RelayerState,
    deposits_tx: mpsc::Sender<String>,
    mut deposits_rx: mpsc::Receiver<String>,
) {
    let semaphore = Arc::new(Semaphore::new(shared.config.forward_concurrency.max(1)));
    // addr -> dirty. Presence means a forward is in flight; `dirty` means another
    // trigger arrived while it was processing, so the address must be re-run once
    // it finishes (a deposit that lands mid-forward must not be lost — the scanner
    // won't re-emit it; the balance-poll backstop would eventually catch it, but
    // re-running immediately avoids waiting a whole poll interval).
    let in_flight: Arc<Mutex<HashMap<String, bool>>> = Arc::new(Mutex::new(HashMap::new()));

    while let Some(addr) = deposits_rx.recv().await {
        {
            let mut guard = in_flight.lock().unwrap();
            if let Some(dirty) = guard.get_mut(&addr) {
                *dirty = true; // already processing; mark for one re-run afterwards
                continue;
            }
            guard.insert(addr.clone(), false);
        }
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break, // semaphore closed -> shutting down
        };
        let shared = shared.clone();
        let in_flight = in_flight.clone();
        let deposits_tx = deposits_tx.clone();
        tokio::spawn(async move {
            forward_address(&shared, &addr).await;
            // Drop the in-flight entry; if a trigger arrived while we were
            // processing, re-enqueue once so the new deposit is handled.
            let requeue = matches!(in_flight.lock().unwrap().remove(&addr), Some(true));
            // Release the concurrency slot before the (bounded, awaiting) re-enqueue
            // so we can't deadlock against a full channel whose only consumer is
            // this dispatcher.
            drop(permit);
            if requeue {
                let _ = deposits_tx.send(addr).await;
            }
        });
    }
}

/// Attempt a single forward for `forward_addr`: resolve its request, honor backoff,
/// check the live balance, and submit. A successful submit forwards the full balance
/// on-chain, so there is exactly one submit per trigger (no residual re-submit).
async fn forward_address(shared: &RelayerState, forward_addr: &str) {
    let request = match shared.live.lock().unwrap().get(forward_addr).cloned() {
        Some(request) => request,
        None => {
            debug!("Skipping {}: no longer in live list", forward_addr);
            return;
        }
    };
    let dest_domain = request.dest_domain.to_string();

    // Honor submission backoff: skip while still inside the wait window.
    if let Some(next_attempt) = shared
        .retry_state
        .lock()
        .unwrap()
        .get(forward_addr)
        .map(|s| s.next_attempt)
    {
        if Utc::now() < next_attempt {
            counter!(
                "forwarding_requests_processed_total",
                "status" => "backoff",
                "dest_domain" => dest_domain.clone()
            )
            .increment(1);
            debug!("Backing off {}", forward_addr);
            return;
        }
    }

    let balances = match shared.celestia.query_balances(forward_addr).await {
        Ok(balances) => balances,
        Err(e) => {
            // A transient query failure must not strand the deposit: schedule a
            // backoff retry (the scanner won't re-emit this deposit; the balance-poll
            // backstop is the slower fallback, but the explicit retry recovers sooner).
            counter!(
                "forwarding_requests_processed_total",
                "status" => "query_failed",
                "dest_domain" => dest_domain
            )
            .increment(1);
            let delay = shared.note_failure(forward_addr);
            error!(
                "Balance query failed for {} (next retry in {}s): {:#}",
                forward_addr,
                delay.as_secs(),
                e
            );
            return;
        }
    };

    if balances.is_empty() {
        counter!(
            "forwarding_requests_processed_total",
            "status" => "empty_balance",
            "dest_domain" => dest_domain
        )
        .increment(1);
        // Nothing to forward; drop any stale backoff so we stop re-attempting.
        shared.clear_retry_state(forward_addr);
        return;
    }

    // A non-empty balance is a deposit: mark the address active and refresh its
    // inactivity timer so it is not retired while it still holds funds.
    shared.record_activity(forward_addr);
    info!("Balance at {}:", forward_addr);
    for balance in &balances {
        info!("  {} {}", balance.amount, balance.denom);
    }

    let max_igp_fee = match resolve_max_igp_fee(shared, &request, &dest_domain).await {
        Some(fee) => fee,
        None => {
            // Same as a balance-query failure: retry with backoff rather than
            // dropping the only trigger for this funded address.
            counter!(
                "forwarding_requests_processed_total",
                "status" => "fee_failed",
                "dest_domain" => dest_domain
            )
            .increment(1);
            let delay = shared.note_failure(forward_addr);
            warn!(
                "IGP fee resolution failed for {} (next retry in {}s)",
                forward_addr,
                delay.as_secs()
            );
            return;
        }
    };

    match shared
        .celestia
        .submit_forward(
            forward_addr,
            request.dest_domain,
            &request.dest_recipient,
            &request.token_id,
            &max_igp_fee,
            shared.config.custom_igp_hook.as_deref(),
        )
        .await
    {
        Ok(tx_hash) => {
            counter!(
                "forwarding_tx_submissions_total",
                "status" => "success",
                "dest_domain" => dest_domain.clone()
            )
            .increment(1);
            counter!(
                "forwarding_requests_processed_total",
                "status" => "submitted",
                "dest_domain" => dest_domain
            )
            .increment(1);
            info!("Forwarding successful: tx_hash={}", tx_hash);

            // Clear backoff and refresh the inactivity timer. The address stays on
            // the live list so future deposits keep being forwarded.
            shared.clear_retry_state(forward_addr);
            shared.record_activity(forward_addr);
        }
        Err(e) => {
            counter!(
                "forwarding_tx_submissions_total",
                "status" => "failure",
                "dest_domain" => dest_domain.clone()
            )
            .increment(1);
            counter!(
                "forwarding_requests_processed_total",
                "status" => "submit_failed",
                "dest_domain" => dest_domain
            )
            .increment(1);
            let delay = shared.note_failure(forward_addr);
            error!(
                "Failed to submit forwarding for {}: {:#} (next retry in {}s)",
                forward_addr,
                e,
                delay.as_secs()
            );
        }
    }
}

/// Resolve the buffered IGP fee for a request, using a short-TTL cache so a domain
/// with many addresses isn't re-quoted on every forward. Returns `None` only if the
/// quote can't be parsed.
async fn resolve_max_igp_fee(
    shared: &RelayerState,
    request: &ForwardingRequest,
    dest_domain_label: &str,
) -> Option<String> {
    let key = (request.dest_domain, request.token_id.clone());

    let cached = {
        let cache = shared.fee_cache.lock().unwrap();
        cache
            .get(&key)
            .and_then(|(fee, at)| (at.elapsed() < FEE_CACHE_TTL).then_some(*fee))
    };

    let quoted_fee_f64 = match cached {
        Some(fee) => fee,
        None => {
            let quoted_fee = match shared
                .celestia
                .query_igp_fee(
                    request.dest_domain,
                    &request.token_id,
                    shared.config.custom_igp_hook.as_deref(),
                )
                .await
            {
                Ok(fee) => fee,
                Err(e) => {
                    error!("IGP fee query failed for {}: {:#}", request.forward_addr, e);
                    return None;
                }
            };
            if let Some(value) = parse_metric_amount(&quoted_fee) {
                gauge!("igp_fee_quote_utia", "dest_domain" => dest_domain_label.to_string())
                    .set(value);
            }
            let parsed: f64 = match quoted_fee.parse() {
                Ok(value) => value,
                Err(e) => {
                    error!("Failed to parse IGP fee '{}': {}", quoted_fee, e);
                    return None;
                }
            };
            shared
                .fee_cache
                .lock()
                .unwrap()
                .insert(key, (parsed, Instant::now()));
            parsed
        }
    };

    let max_fee = (quoted_fee_f64 * shared.config.igp_fee_buffer) as u64;
    Some(format!("{}utia", max_fee))
}

/// Periodic maintenance: refresh the live list from the backend, retire expired
/// addresses, prune state, probe newly-added addresses, and re-enqueue retries.
async fn run_maintenance(shared: RelayerState, deposits_tx: mpsc::Sender<String>) {
    let interval = Duration::from_secs(shared.config.maintenance_interval.max(1));
    loop {
        if let Err(e) = maintenance_tick(&shared, &deposits_tx).await {
            warn!("Maintenance tick failed: {:#}", e);
        }
        tokio::time::sleep(interval).await;
    }
}

/// Periodic balance-poll backstop. Every `balance_poll_interval` seconds it
/// re-enqueues every address currently on the live list for a forward attempt.
///
/// This is deliberately the same path a detected deposit takes, so it inherits all
/// the existing safety: the dispatcher dedupes addresses already in flight, the
/// forward path skips addresses still inside their submission-backoff window, and an
/// address with an empty balance is a cheap no-op that clears stale state. The net
/// effect is that detection no longer depends on catching the exact block a deposit
/// landed in — a scanner miss merely delays the forward to the next sweep.
async fn run_balance_poll(shared: RelayerState, deposits_tx: mpsc::Sender<String>) {
    let interval = Duration::from_secs(shared.config.balance_poll_interval.max(1));
    info!(
        "Balance-poll backstop enabled: re-checking live addresses every {}s",
        interval.as_secs()
    );
    loop {
        tokio::time::sleep(interval).await;

        // Snapshot the live keys, then release the lock before the (async, bounded)
        // sends so we never hold the mutex across an `.await`.
        let addrs: Vec<String> = shared.live.lock().unwrap().keys().cloned().collect();
        let count = addrs.len();
        for addr in addrs {
            // Awaiting applies backpressure if the forward queue is full, matching
            // the scanner's behavior; a closed receiver only happens on shutdown.
            if deposits_tx.send(addr).await.is_err() {
                return;
            }
        }
        counter!("relayer_balance_poll_sweeps_total").increment(1);
        gauge!("relayer_balance_poll_addresses").set(count as f64);
        debug!("Balance-poll backstop swept {count} live addresses");
    }
}

async fn maintenance_tick(shared: &RelayerState, deposits_tx: &mpsc::Sender<String>) -> Result<()> {
    let requests = match shared.fetch_forwarding_requests().await {
        Ok(reqs) => {
            counter!("backend_request_fetch_total", "status" => "success").increment(1);
            gauge!("forwarding_requests_fetched").set(reqs.len() as f64);
            reqs
        }
        Err(e) => {
            counter!("backend_request_fetch_total", "status" => "failure").increment(1);
            warn!("Failed to fetch forwarding requests from backend: {:#}", e);
            return Ok(()); // keep the existing live list until the backend recovers
        }
    };

    let now = Utc::now();

    // Decide retirements using a single snapshot of activity timestamps.
    let to_retire: Vec<(ForwardingRequest, RetireReason)> = {
        let last_activity = shared.last_activity.lock().unwrap();
        requests
            .iter()
            .filter_map(|req| {
                retirement_reason(
                    &req.created_at,
                    last_activity.get(&req.forward_addr).copied(),
                    now,
                    shared.config.max_request_age_seconds,
                    shared.config.max_address_inactivity_seconds,
                )
                .map(|reason| (req.clone(), reason))
            })
            .collect()
    };

    let retired: HashSet<&str> = to_retire
        .iter()
        .map(|(req, _)| req.forward_addr.as_str())
        .collect();

    // Build the new live map (kept = fetched minus retired) and find newly-added
    // addresses (not previously live) to probe once for pre-existing balances.
    let kept: HashMap<String, ForwardingRequest> = requests
        .iter()
        .filter(|req| !retired.contains(req.forward_addr.as_str()))
        .map(|req| (req.forward_addr.clone(), req.clone()))
        .collect();

    let newly_added: Vec<String> = {
        let prev = shared.live.lock().unwrap();
        kept.keys()
            .filter(|addr| !prev.contains_key(*addr))
            .cloned()
            .collect()
    };

    // Retire expired addresses (backend DELETE + clear local state).
    for (req, reason) in &to_retire {
        let addr = &req.forward_addr;
        counter!("relayer_addresses_retired_total", "reason" => reason.label()).increment(1);
        shared.clear_retry_state(addr);
        shared.clear_activity(addr);
        if let Err(e) = shared.complete_request(addr).await {
            warn!("Failed to retire request for {}: {:#}", addr, e);
        } else {
            warn!(
                "Retired request ({}): forward_addr={} dest_domain={} created_at={} {}",
                reason.label(),
                addr,
                req.dest_domain,
                req.created_at,
                reason.detail(),
            );
        }
    }

    // Swap in the new live list and prune state to it.
    let keep_set: HashSet<&str> = kept.keys().map(|s| s.as_str()).collect();
    shared
        .retry_state
        .lock()
        .unwrap()
        .retain(|addr, _| keep_set.contains(addr.as_str()));
    shared
        .last_activity
        .lock()
        .unwrap()
        .retain(|addr, _| keep_set.contains(addr.as_str()));
    if let Err(e) = shared.store.lock().unwrap().retain(&keep_set) {
        warn!("Failed to prune persisted state: {:#}", e);
    }
    gauge!("relayer_live_addresses").set(kept.len() as f64);
    *shared.live.lock().unwrap() = kept;

    // Probe newly-added addresses once (catches funds deposited before
    // registration, and on first run probes the entire restored live list).
    for addr in newly_added {
        let _ = deposits_tx.send(addr).await;
    }

    // Re-enqueue addresses whose backoff window has elapsed.
    let due: Vec<String> = {
        let retry = shared.retry_state.lock().unwrap();
        retry
            .iter()
            .filter(|(_, state)| now >= state.next_attempt)
            .map(|(addr, _)| addr.clone())
            .collect()
    };
    for addr in due {
        let _ = deposits_tx.send(addr).await;
    }

    Ok(())
}

async fn refresh_signer_balance_metrics(shared: &RelayerState) -> Result<()> {
    let signer_address = shared.celestia.signer_address().to_string();
    let balances = shared.celestia.query_balances(&signer_address).await?;
    let utia_balance = balances
        .into_iter()
        .find(|balance| balance.denom == "utia")
        .and_then(|balance| parse_metric_amount(&balance.amount))
        .unwrap_or(0.0);

    gauge!("signer_balance", "denom" => "utia").set(utia_balance);
    Ok(())
}

/// Why an address should be retired from the live monitoring list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireReason {
    /// Never saw a deposit and outlived `max_request_age_seconds`.
    Unfunded { age: i64 },
    /// Saw activity before but has been idle past `max_address_inactivity_seconds`.
    Inactive { idle: i64 },
}

impl RetireReason {
    /// Short, stable label for metrics/logs.
    pub fn label(&self) -> &'static str {
        match self {
            RetireReason::Unfunded { .. } => "unfunded",
            RetireReason::Inactive { .. } => "inactive",
        }
    }

    /// Human-readable detail for logs.
    pub fn detail(&self) -> String {
        match self {
            RetireReason::Unfunded { age } => format!("age={age}s (never funded)"),
            RetireReason::Inactive { idle } => format!("idle={idle}s"),
        }
    }
}

/// Decide whether an address should be retired from the live list.
///
/// An address that has never seen activity (`last_activity == None`) is retired
/// once its age exceeds `max_request_age_seconds`. Once it has seen a deposit or
/// forward (`last_activity == Some`), it is instead retired once it has been idle
/// past `max_address_inactivity_seconds`. Returns `None` if it should be kept.
pub fn retirement_reason(
    created_at: &str,
    last_activity: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    max_request_age_seconds: u64,
    max_address_inactivity_seconds: u64,
) -> Option<RetireReason> {
    match last_activity {
        None => {
            let created = DateTime::parse_from_rfc3339(created_at).ok()?;
            let age = now
                .signed_duration_since(created.with_timezone(&Utc))
                .num_seconds()
                .max(0);
            (age > max_request_age_seconds as i64).then_some(RetireReason::Unfunded { age })
        }
        Some(last) => {
            let idle = now.signed_duration_since(last).num_seconds().max(0);
            (idle > max_address_inactivity_seconds as i64)
                .then_some(RetireReason::Inactive { idle })
        }
    }
}

pub fn parse_metric_amount(value: &str) -> Option<f64> {
    match value.parse::<f64>() {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            warn!("Failed to parse metric amount '{value}': {err}");
            None
        }
    }
}
