use anyhow::{Context, Result};
use clap::Parser;
use reqwest::Client as HttpClient;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::client::CelestiaClient;
use crate::{Balance, ForwardingRequest, StatusUpdate};

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
}

/// Relayer state
pub struct Relayer {
    config: RelayerConfig,
    celestia: CelestiaClient,
    http_client: HttpClient,
}

impl Relayer {
    pub async fn new(config: RelayerConfig) -> Result<Self> {
        let celestia =
            CelestiaClient::new(config.celestia_grpc.clone(), config.private_key_hex.clone())
                .await?;

        info!("Relayer address: {}", celestia.signer_address());

        Ok(Self {
            config,
            celestia,
            http_client: HttpClient::new(),
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
        info!("Celestia gRPC: {}", self.config.celestia_grpc);
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

        if balances.is_empty() {
            debug!("No balance at {}", forward_addr);
            return Ok(());
        }

        info!("Balance at {}:", forward_addr);
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

                if let Err(e) = self.update_request_status(&request.id, "completed").await {
                    warn!(
                        "Failed to update backend status for request {}: {:#}",
                        request.id, e
                    );
                }
            }
            Err(e) => {
                error!("Failed to submit forwarding for {}: {:#}", forward_addr, e);
            }
        }

        Ok(())
    }
}

