use anyhow::{Context, Result};
use clap::Parser;
use reqwest::Client as HttpClient;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::client::CelestiaClient;
use crate::{Balance, ForwardingRequest, StatusUpdate};

/// Relayer configuration
#[derive(Parser, Debug)]
pub struct RelayerConfig {
    /// Celestia Tendermint RPC URL (port 26657)
    #[arg(long, env = "CELESTIA_RPC", default_value = "http://localhost:26657")]
    pub celestia_rpc: String,

    /// Celestia gRPC URL (port 9090)
    #[arg(long, env = "CELESTIA_GRPC", default_value = "http://localhost:9090")]
    pub celestia_grpc: String,

    /// Backend API URL
    #[arg(long, env = "BACKEND_URL", default_value = "http://localhost:8080")]
    pub backend_url: String,

    /// Relayer mnemonic (for signing transactions)
    #[arg(long, env = "RELAYER_MNEMONIC")]
    pub relayer_mnemonic: String,

    /// Celestia chain ID
    #[arg(long, env = "CHAIN_ID", default_value = "celestia-zkevm-testnet")]
    pub chain_id: String,

    /// Poll interval in seconds
    #[arg(long, env = "POLL_INTERVAL", default_value = "6")]
    pub poll_interval: u64,

    /// IGP fee buffer multiplier (e.g., 1.1 for 10% buffer)
    #[arg(long, env = "IGP_FEE_BUFFER", default_value = "1.1")]
    pub igp_fee_buffer: f64,

    /// Path to balance cache database file
    #[arg(
        long,
        env = "BALANCE_CACHE_PATH",
        default_value = "storage/balance_cache.db"
    )]
    pub balance_cache_path: PathBuf,
}

/// SQLite storage for balance cache (used by relayer)
pub struct BalanceCacheStorage {
    conn: Arc<Mutex<Connection>>,
}

impl BalanceCacheStorage {
    /// Create or open balance cache database
    pub fn new(db_path: &StdPath) -> Result<Self> {
        // Create parent directory if it doesn't exist
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {:?}", parent))?;
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("Failed to open balance cache DB at {:?}", db_path))?;

        // Create table if it doesn't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS balance_cache (
                address TEXT PRIMARY KEY,
                balances TEXT NOT NULL
            )",
            [],
        )
        .context("Failed to create balance_cache table")?;

        info!("Opened balance cache database at {:?}", db_path);

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Load all cached balances
    pub fn load_all(&self) -> Result<HashMap<String, Vec<Balance>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT address, balances FROM balance_cache")
            .context("Failed to prepare SELECT statement")?;

        let mut cache = HashMap::new();
        let rows = stmt
            .query_map([], |row| {
                let address: String = row.get(0)?;
                let balances_json: String = row.get(1)?;
                Ok((address, balances_json))
            })
            .context("Failed to query balance cache")?;

        for row in rows {
            let (address, balances_json) = row.context("Failed to read row")?;
            let balances: Vec<Balance> =
                serde_json::from_str(&balances_json).context("Failed to deserialize balances")?;
            cache.insert(address, balances);
        }

        Ok(cache)
    }

    /// Save balance for a specific address
    pub fn save(&self, address: &str, balances: &[Balance]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let balances_json =
            serde_json::to_string(balances).context("Failed to serialize balances")?;

        conn.execute(
            "INSERT OR REPLACE INTO balance_cache (address, balances) VALUES (?1, ?2)",
            params![address, balances_json],
        )
        .context("Failed to save balance to cache")?;

        Ok(())
    }

    /// Remove balance cache for a specific address
    pub fn remove(&self, address: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM balance_cache WHERE address = ?1",
            params![address],
        )
        .context("Failed to remove balance from cache")?;
        Ok(())
    }
}

/// Relayer state
pub struct Relayer {
    config: RelayerConfig,
    celestia: CelestiaClient,
    http_client: HttpClient,
    balance_cache: BalanceCacheStorage,
    cached_balances: HashMap<String, Vec<Balance>>,
}

impl Relayer {
    pub async fn new(config: RelayerConfig) -> Result<Self> {
        // CELESTIA_RPC is now the Tendermint RPC URL (port 26657)
        // All queries use ABCI queries via Tendermint RPC
        let celestia = CelestiaClient::new(
            config.celestia_rpc.clone(), // kept for compatibility, not used
            config.celestia_rpc.clone(), // Tendermint RPC URL
            config.celestia_grpc.clone(),
            config.relayer_mnemonic.clone(),
            config.chain_id.clone(),
        )
        .await?;

        info!("Relayer address: {}", celestia.signer_address);

        // Open balance cache database
        let balance_cache = BalanceCacheStorage::new(&config.balance_cache_path)?;
        let cached_balances = balance_cache.load_all()?;
        info!(
            "Loaded balance cache with {} addresses from database",
            cached_balances.len()
        );

        Ok(Self {
            config,
            celestia,
            http_client: HttpClient::new(),
            balance_cache,
            cached_balances,
        })
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

    /// Update forwarding request status in backend
    async fn update_request_status(&self, request_id: &str, status: &str) -> Result<()> {
        let url = format!(
            "{}/forwarding-requests/{}/status",
            self.config.backend_url, request_id
        );

        let update = StatusUpdate {
            status: status.to_string(),
        };

        let response = self
            .http_client
            .patch(&url)
            .json(&update)
            .send()
            .await
            .with_context(|| format!("Failed to update status for request {}", request_id))?;

        if !response.status().is_success() {
            warn!(
                "Failed to update backend status for request {}: {}",
                request_id,
                response.status()
            );
        } else {
            info!(
                "Updated backend status for request {} to {}",
                request_id, status
            );
        }

        Ok(())
    }

    /// Main relayer loop
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting forwarding relayer");
        info!("Celestia RPC: {}", self.config.celestia_rpc);
        info!("Backend URL: {}", self.config.backend_url);
        info!("Poll interval: {}s", self.config.poll_interval);

        let poll_interval = Duration::from_secs(self.config.poll_interval);

        loop {
            if let Err(e) = self.process_round().await {
                error!("Error in relayer round: {:#}", e);
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Process one round of forwarding
    async fn process_round(&mut self) -> Result<()> {
        // Fetch forwarding requests from backend
        let requests = match self.fetch_forwarding_requests().await {
            Ok(reqs) => reqs,
            Err(e) => {
                warn!("Failed to fetch forwarding requests from backend: {:#}", e);
                // Continue with empty list if backend is unavailable
                Vec::new()
            }
        };

        debug!("Processing {} forwarding requests", requests.len());

        // Process each forwarding request
        for request in requests {
            if request.status != "pending" {
                continue;
            }

            if let Err(e) = self.process_forwarding_request(&request).await {
                error!(
                    "Error processing forwarding request for {}: {:#}",
                    request.forward_addr, e
                );
            }
        }

        Ok(())
    }

    /// Process a single forwarding request
    async fn process_forwarding_request(&mut self, request: &ForwardingRequest) -> Result<()> {
        let forward_addr = &request.forward_addr;

        debug!("Checking balance at {}", forward_addr);

        // Query current balance
        let balances = self.celestia.query_balances(forward_addr).await?;

        // Get cached balance for this address
        let cached_balance = self.cached_balances.get(forward_addr);

        // Check if balance has changed (gone up)
        let balance_increased = !balances.is_empty()
            && (cached_balance.is_none() || !balances_equal(cached_balance.unwrap(), &balances));

        if !balance_increased {
            debug!("No new deposits detected at {}", forward_addr);
            return Ok(());
        }

        if balances.is_empty() {
            debug!("No balance at forwarding address {}", forward_addr);
            self.cached_balances
                .insert(forward_addr.clone(), balances.clone());
            self.balance_cache.save(forward_addr, &balances)?;
            return Ok(());
        }

        info!("New deposit detected at {}! Balance changed:", forward_addr);
        for balance in &balances {
            info!("  {} {}", balance.amount, balance.denom);
        }

        // Query IGP fee and apply buffer
        let quoted_fee = self.celestia.query_igp_fee(request.dest_domain).await?;
        let quoted_fee_f64: f64 = quoted_fee.parse().context("Failed to parse IGP fee")?;
        let max_fee = (quoted_fee_f64 * self.config.igp_fee_buffer) as u64;
        let max_igp_fee = format!("{}utia", max_fee);

        info!(
            "IGP fee for domain {}: quoted={}, max={} ({}x buffer)",
            request.dest_domain, quoted_fee, max_igp_fee, self.config.igp_fee_buffer
        );

        // Update balance cache BEFORE submitting transaction to prevent duplicate submissions
        // This ensures that even if the transaction is pending in mempool, we won't retry
        self.cached_balances
            .insert(forward_addr.clone(), balances.clone());
        self.balance_cache.save(forward_addr, &balances)?;

        // Submit forwarding transaction
        match self
            .celestia
            .submit_forward(
                forward_addr,
                request.dest_domain,
                &request.dest_recipient,
                &max_igp_fee,
            )
            .await
        {
            Ok(tx_hash) => {
                info!("Forwarding successful: tx_hash={}", tx_hash);

                // Transaction succeeded, update backend status to completed
                if let Err(e) = self.update_request_status(&request.id, "completed").await {
                    warn!(
                        "Failed to update backend status for request {}: {:#}",
                        request.id, e
                    );
                }

                // Clear balance cache for this address now that forwarding is complete
                self.cached_balances.remove(forward_addr);
                self.balance_cache.remove(forward_addr)?;
            }
            Err(e) => {
                error!("Failed to submit forwarding for {}: {:#}", forward_addr, e);
                // Note: We keep the updated balance cache to prevent retrying the same transaction
            }
        }

        Ok(())
    }
}

/// Check if two balance sets are equal
pub fn balances_equal(a: &[Balance], b: &[Balance]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut a_map: HashMap<&str, &str> = HashMap::new();
    for balance in a {
        a_map.insert(&balance.denom, &balance.amount);
    }

    for balance in b {
        match a_map.get(balance.denom.as_str()) {
            Some(&amount) if amount == balance.amount => {}
            _ => return false,
        }
    }

    true
}
