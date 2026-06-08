use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use metrics::{counter, gauge};
use reqwest::Client as HttpClient;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::client::CelestiaClient;
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

/// SQLite-backed persistence for per-request submission backoff, so retry
/// progress survives a relayer crash or restart instead of resetting to an
/// immediate re-attempt for every pending request.
struct RetryStore {
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

        info!("Opened relayer retry-state database at {:?}", db_path);

        Ok(Self { conn })
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

    /// Delete persisted state for any address not in `active` (no longer pending).
    fn retain(&self, active: &HashSet<&str>) -> Result<()> {
        let mut stmt = self.conn.prepare("SELECT forward_addr FROM retry_state")?;
        let addrs: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        for addr in addrs {
            if !active.contains(addr.as_str()) {
                self.remove(&addr)?;
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
    /// Celestia gRPC URL (port 9090)
    #[arg(long, env = "CELESTIA_GRPC", default_value = "http://localhost:9090")]
    pub celestia_grpc: String,

    /// Backend API URL
    #[arg(long, env = "BACKEND_URL", default_value = "http://localhost:8080")]
    pub backend_url: String,

    /// Relayer secp256k1 private key hex (for signing transactions)
    #[arg(long, env = "PRIVATE_KEY_HEX")]
    pub private_key_hex: String,

    /// Poll interval in seconds
    #[arg(long, env = "POLL_INTERVAL", default_value = "6")]
    pub poll_interval: u64,

    /// IGP fee buffer multiplier (e.g., 1.1 for 10% buffer)
    #[arg(long, env = "IGP_FEE_BUFFER", default_value = "1.1")]
    pub igp_fee_buffer: f64,

    /// Maximum age for a forwarding request in seconds before it's considered dead (default: 86400 = 1 day)
    #[arg(long, env = "MAX_REQUEST_AGE_SECONDS", default_value = "86400")]
    pub max_request_age_seconds: u64,

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

/// Relayer state
pub struct Relayer {
    config: RelayerConfig,
    celestia: CelestiaClient,
    http_client: HttpClient,
    /// In-memory cache of per-request submission backoff, keyed by forward
    /// address. Mirrored to `retry_store` so it survives restarts.
    retry_state: HashMap<String, RetryState>,
    /// Durable backing store for `retry_state`.
    retry_store: RetryStore,
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

        let retry_store = RetryStore::new(&config.retry_state_db_path)?;
        let retry_state = retry_store.load().unwrap_or_else(|e| {
            warn!("Failed to load persisted retry state, starting fresh: {:#}", e);
            HashMap::new()
        });
        info!("Loaded {} persisted retry-state entries", retry_state.len());

        Ok(Self {
            config,
            celestia,
            http_client,
            retry_state,
            retry_store,
        })
    }

    /// Main relayer loop
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting forwarding relayer");
        info!("Celestia gRPC: {}", self.config.celestia_grpc);
        info!("Backend URL: {}", self.config.backend_url);
        info!("Poll interval: {}s", self.config.poll_interval);

        let poll_interval = Duration::from_secs(self.config.poll_interval);

        loop {
            match self.process_round().await {
                Ok(()) => {
                    counter!("relayer_rounds_total", "status" => "ok").increment(1);
                }
                Err(e) => {
                    counter!("relayer_rounds_total", "status" => "error").increment(1);
                    error!("Error in relayer round: {:#}", e);
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Process one round of forwarding
    async fn process_round(&mut self) -> Result<()> {
        self.refresh_signer_balance_metrics().await?;

        // Fetch forwarding requests from backend
        let requests = match self.fetch_forwarding_requests().await {
            Ok(reqs) => {
                counter!("backend_request_fetch_total", "status" => "success").increment(1);
                gauge!("forwarding_requests_fetched").set(reqs.len() as f64);
                reqs
            }
            Err(e) => {
                counter!("backend_request_fetch_total", "status" => "failure").increment(1);
                gauge!("forwarding_requests_fetched").set(0.0);
                warn!("Failed to fetch forwarding requests from backend: {:#}", e);
                // Continue with empty list if backend is unavailable
                Vec::new()
            }
        };

        debug!("Processing {} forwarding requests", requests.len());

        // Process each forwarding request
        for request in &requests {
            if let Err(e) = self.process_forwarding_request(request).await {
                error!(
                    "Error processing forwarding request for {}: {:#}",
                    request.forward_addr, e
                );
            }
        }

        // Drop backoff state for requests no longer pending (completed, dropped,
        // or removed out-of-band) so the map cannot grow without bound.
        let active: HashSet<&str> = requests.iter().map(|r| r.forward_addr.as_str()).collect();
        self.retry_state
            .retain(|addr, _| active.contains(addr.as_str()));
        if let Err(e) = self.retry_store.retain(&active) {
            warn!("Failed to prune persisted retry state: {:#}", e);
        }

        Ok(())
    }

    /// Process a single forwarding request
    async fn process_forwarding_request(&mut self, request: &ForwardingRequest) -> Result<()> {
        let forward_addr = &request.forward_addr;
        let dest_domain = request.dest_domain.to_string();

        // Drop requests that have outlived the configured max age
        if let Some(age) = self.expired_age(request) {
            self.clear_retry_state(forward_addr);
            match self.complete_request(forward_addr).await {
                Ok(_) => warn!(
                    "Dropped dead request: forward_addr={} dest_domain={} dest_recipient={} token_id={} created_at={} age={}s",
                    forward_addr,
                    request.dest_domain,
                    request.dest_recipient,
                    request.token_id,
                    request.created_at,
                    age,
                ),
                Err(e) => warn!("Failed to drop dead request for {}: {:#}", forward_addr, e),
            }
            return Ok(());
        }

        // Honor submission backoff: skip the request entirely (no gRPC work, no
        // tx) while it is still within the wait window from a prior failure.
        if let Some(state) = self.retry_state.get(forward_addr) {
            let now = Utc::now();
            if now < state.next_attempt {
                let wait = (state.next_attempt - now).num_seconds().max(0);
                counter!(
                    "forwarding_requests_processed_total",
                    "status" => "backoff",
                    "dest_domain" => dest_domain.clone()
                )
                .increment(1);
                debug!(
                    "Backing off {} (failures={}, retry in ~{}s)",
                    forward_addr, state.failures, wait
                );
                return Ok(());
            }
        }

        debug!("Checking balance at {}", forward_addr);

        // Query current balance
        let balances = self.celestia.query_balances(forward_addr).await?;

        if balances.is_empty() {
            counter!(
                "forwarding_requests_processed_total",
                "status" => "empty_balance",
                "dest_domain" => dest_domain.clone()
            )
            .increment(1);
            debug!("No balance at {}", forward_addr);
            return Ok(());
        }

        info!("Balance at {}:", forward_addr);
        for balance in &balances {
            info!("  {} {}", balance.amount, balance.denom);
        }

        // Query IGP fee and apply buffer
        let quoted_fee = self
            .celestia
            .query_igp_fee(request.dest_domain, &request.token_id)
            .await?;
        if let Some(quoted_fee_value) = parse_metric_amount(&quoted_fee) {
            gauge!("igp_fee_quote_utia", "dest_domain" => dest_domain.clone())
                .set(quoted_fee_value);
        }
        let quoted_fee_f64: f64 = quoted_fee.parse().context("Failed to parse IGP fee")?;
        let max_fee = (quoted_fee_f64 * self.config.igp_fee_buffer) as u64;
        let max_igp_fee = format!("{}utia", max_fee);

        info!(
            "IGP fee for domain {}: quoted={}, max={} ({}x buffer)",
            request.dest_domain, quoted_fee, max_igp_fee, self.config.igp_fee_buffer
        );

        // Submit forwarding transaction
        match self
            .celestia
            .submit_forward(
                forward_addr,
                request.dest_domain,
                &request.dest_recipient,
                &request.token_id,
                &max_igp_fee,
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

                // Successful submission clears any accumulated backoff.
                self.clear_retry_state(forward_addr);

                if let Err(e) = self.complete_request(forward_addr).await {
                    warn!(
                        "Failed to remove backend request for {}: {:#}",
                        forward_addr, e
                    );
                }
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

                // Advance the backoff so the next attempt is delayed; the delay
                // grows per the schedule and saturates at 1 hour, while the
                // max-age check above still drops the request once it expires.
                let failures = self
                    .retry_state
                    .get(forward_addr)
                    .map_or(0, |s| s.failures)
                    + 1;
                let delay = retry_delay(failures);
                let next_attempt = Utc::now()
                    + chrono::Duration::from_std(delay)
                        .unwrap_or_else(|_| chrono::Duration::seconds(3600));
                let state = RetryState {
                    failures,
                    next_attempt,
                };
                if let Err(e) = self.retry_store.upsert(forward_addr, &state) {
                    warn!("Failed to persist retry state for {}: {:#}", forward_addr, e);
                }
                self.retry_state.insert(forward_addr.to_string(), state);
                error!(
                    "Failed to submit forwarding for {}: {:#} (failure #{}, next retry in {}s)",
                    forward_addr,
                    e,
                    failures,
                    delay.as_secs()
                );
            }
        }

        Ok(())
    }

    async fn refresh_signer_balance_metrics(&mut self) -> Result<()> {
        let signer_address = self.celestia.signer_address().to_string();
        let balances = self.celestia.query_balances(&signer_address).await?;
        let utia_balance = balances
            .into_iter()
            .find(|balance| balance.denom == "utia")
            .and_then(|balance| parse_metric_amount(&balance.amount))
            .unwrap_or(0.0);

        gauge!("signer_balance", "denom" => "utia").set(utia_balance);

        Ok(())
    }

    /// Fetch forwarding requests from backend
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

        let requests = response
            .json::<Vec<ForwardingRequest>>()
            .await
            .context("Failed to parse forwarding requests")?;

        Ok(requests)
    }

    /// Notify backend that forwarding for an address completed (removes the pending request)
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

    /// Clear any backoff state for an address from both the in-memory cache and
    /// the durable store (on successful submission or when dropping the request).
    fn clear_retry_state(&mut self, forward_addr: &str) {
        self.retry_state.remove(forward_addr);
        if let Err(e) = self.retry_store.remove(forward_addr) {
            warn!(
                "Failed to clear persisted retry state for {}: {:#}",
                forward_addr, e
            );
        }
    }

    /// Returns the age in seconds if `request` has exceeded `max_request_age_seconds`.
    fn expired_age(&self, request: &ForwardingRequest) -> Option<i64> {
        let age = calculate_request_age(&request.created_at).ok()?;
        (age > self.config.max_request_age_seconds as i64).then_some(age)
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

/// Calculate the age of a forwarding request in seconds from its created_at timestamp.
fn calculate_request_age(created_at: &str) -> Result<i64> {
    let created = chrono::DateTime::parse_from_rfc3339(created_at)
        .with_context(|| format!("Invalid request timestamp: {created_at}"))?;
    let age = chrono::Utc::now()
        .signed_duration_since(created.with_timezone(&chrono::Utc))
        .num_seconds();
    Ok(age.max(0))
}
