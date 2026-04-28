use anyhow::{Context, Result};
use clap::Parser;
use metrics::{counter, gauge};
use reqwest::Client as HttpClient;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::client::CelestiaClient;
use crate::{Balance, ForwardingRequest};

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

    /// Maximum age for a forwarding request in seconds before it's considered dead (default: 604800 = 7 days)
    #[arg(long, env = "MAX_REQUEST_AGE_SECONDS", default_value = "604800")]
    pub max_request_age_seconds: u64,

    /// Metrics port for Prometheus scraping
    #[arg(long, env = "RELAYER_METRICS_PORT")]
    pub metrics_port: Option<u16>,
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

        let http_client = HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            config,
            celestia,
            http_client,
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
        for request in requests {
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
        let dest_domain = request.dest_domain.to_string();

        // Drop requests that have outlived the configured max age
        if let Some(age) = self.expired_age(request) {
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
                error!("Failed to submit forwarding for {}: {:#}", forward_addr, e);
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
