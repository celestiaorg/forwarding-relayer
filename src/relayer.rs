use anyhow::{Context, Result};
use clap::Parser;
use reqwest::Client as HttpClient;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::client::CelestiaClient;
use crate::metrics::{classify_error, spawn_metrics_server, RelayerMetrics};
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

    /// Address for the dedicated Prometheus metrics listener
    #[arg(long, env = "RELAYER_METRICS_BIND")]
    pub metrics_bind: Option<String>,
}

/// Relayer state
pub struct Relayer {
    config: RelayerConfig,
    celestia: CelestiaClient,
    http_client: HttpClient,
    metrics: RelayerMetrics,
}

impl Relayer {
    /// Create a relayer with real backend and Celestia clients.
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
            metrics: RelayerMetrics::new()?,
        })
    }

    /// Fetch forwarding requests from the backend and record backend-call metrics.
    async fn fetch_forwarding_requests(&self) -> Result<Vec<ForwardingRequest>> {
        let url = format!("{}/forwarding-requests", self.config.backend_url);
        let start = Instant::now();

        let result = async {
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
        .await;

        let _elapsed = start.elapsed();
        match &result {
            Ok(requests) => {
                self.metrics
                    .observe_backend_call("fetch_requests", "success");
                self.metrics.set_requests_fetched(requests.len());
            }
            Err(err) => {
                self.metrics
                    .observe_backend_call("fetch_requests", classify_error(err));
            }
        }

        result
    }

    /// Notify the backend that forwarding for an address completed.
    async fn complete_request(&self, forward_addr: &str) -> Result<()> {
        let url = format!(
            "{}/forwarding-requests/{}",
            self.config.backend_url, forward_addr
        );
        let start = Instant::now();

        let result = async {
            let response = self
                .http_client
                .delete(&url)
                .send()
                .await
                .with_context(|| format!("Failed to complete request for {forward_addr}"))?;

            if !response.status().is_success() {
                anyhow::bail!(
                    "Backend completion returned error for {}: {}",
                    forward_addr,
                    response.status()
                );
            }

            Ok(())
        }
        .await;

        let _elapsed = start.elapsed();
        match &result {
            Ok(()) => {
                self.metrics
                    .observe_backend_call("complete_request", "success");
                info!("Removed completed request for address {}", forward_addr);
            }
            Err(err) => {
                self.metrics
                    .observe_backend_call("complete_request", classify_error(err));
            }
        }

        result
    }

    /// Query balances for a forwarding address and record Celestia-call metrics.
    async fn query_balances(&self, address: &str) -> Result<Vec<Balance>> {
        let result = self.celestia.query_balances(address).await;

        match &result {
            Ok(_) => {
                self.metrics
                    .observe_celestia_call("query_balances", "success");
            }
            Err(err) => {
                self.metrics
                    .observe_celestia_call("query_balances", classify_error(err));
            }
        }

        result
    }

    /// Refresh the relayer signer's balance gauges.
    async fn refresh_signer_balance(&self) -> Result<()> {
        let signer_address = self.celestia.signer_address().to_string();
        let result = self.celestia.query_balances(&signer_address).await;

        match &result {
            Ok(balances) => {
                self.metrics
                    .observe_celestia_call("query_signer_balance", "success");
                self.metrics.update_signer_balance(balances);
            }
            Err(err) => {
                self.metrics
                    .observe_celestia_call("query_signer_balance", classify_error(err));
            }
        }

        result.map(|_| ())
    }

    /// Query the latest IGP fee for a destination domain and update the fee gauge.
    async fn query_igp_fee(&self, dest_domain: u32) -> Result<String> {
        let result = self.celestia.query_igp_fee(dest_domain).await;

        match &result {
            Ok(quoted_fee) => {
                self.metrics
                    .observe_celestia_call("query_igp_fee", "success");
                if let Ok(parsed) = quoted_fee.parse::<u64>() {
                    self.metrics.set_igp_fee_quote(dest_domain, parsed);
                }
            }
            Err(err) => {
                self.metrics
                    .observe_celestia_call("query_igp_fee", classify_error(err));
            }
        }

        result
    }

    /// Submit a forwarding transaction and update both submission and Celestia-call metrics.
    async fn submit_forward(
        &self,
        forward_addr: &str,
        dest_domain: u32,
        dest_recipient: &str,
        max_igp_fee: &str,
    ) -> Result<String> {
        let start = Instant::now();
        let result = self
            .celestia
            .submit_forward(forward_addr, dest_domain, dest_recipient, max_igp_fee)
            .await;
        let elapsed = start.elapsed();

        match &result {
            Ok(_) => {
                self.metrics
                    .observe_celestia_call("submit_forward", "success");
                self.metrics
                    .observe_forwarding_attempt("submitted", elapsed);
                self.metrics.mark_successful_forward();
            }
            Err(err) => {
                let classified = classify_error(err);
                self.metrics
                    .observe_celestia_call("submit_forward", classified);
                self.metrics
                    .observe_forwarding_attempt("submit_error", elapsed);
            }
        }

        result
    }

    /// Main relayer loop
    pub async fn run(&mut self) -> Result<()> {
        if let Some(bind) = self.config.metrics_bind.as_deref() {
            let _metrics_server = spawn_metrics_server(bind, self.metrics.registry()).await?;
            info!("Relayer metrics listening on {}", bind);
        }

        info!("Starting forwarding relayer");
        info!("Celestia gRPC: {}", self.config.celestia_grpc);
        info!("Backend URL: {}", self.config.backend_url);
        info!("Poll interval: {}s", self.config.poll_interval);

        let poll_interval = Duration::from_secs(self.config.poll_interval);

        loop {
            let round_start = Instant::now();
            match self.process_round().await {
                Ok(()) => {
                    self.metrics.observe_round("ok", round_start.elapsed());
                    self.metrics.mark_successful_round();
                }
                Err(err) => {
                    self.metrics.observe_round("error", round_start.elapsed());
                    error!("Error in relayer round: {:#}", err);
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    /// Process one polling round across all currently pending forwarding requests.
    async fn process_round(&mut self) -> Result<()> {
        let mut had_error = false;

        let requests = match self.fetch_forwarding_requests().await {
            Ok(requests) => requests,
            Err(err) => {
                warn!(
                    "Failed to fetch forwarding requests from backend: {:#}",
                    err
                );
                had_error = true;
                Vec::new()
            }
        };

        if let Err(err) = self.refresh_signer_balance().await {
            warn!("Failed to query relayer signer balance: {:#}", err);
            had_error = true;
        }

        debug!("Processing {} forwarding requests", requests.len());

        for request in requests {
            if let Err(err) = self.process_forwarding_request(&request).await {
                error!(
                    "Error processing forwarding request for {}: {:#}",
                    request.forward_addr, err
                );
                had_error = true;
            }
        }

        if had_error {
            anyhow::bail!("one or more operations failed during relayer round");
        }

        Ok(())
    }

    /// Process a single forwarding request from balance detection through completion.
    async fn process_forwarding_request(&mut self, request: &ForwardingRequest) -> Result<()> {
        let forward_addr = &request.forward_addr;
        debug!("Checking balance at {}", forward_addr);

        let balances = self.query_balances(forward_addr).await?;
        if balances.is_empty() {
            debug!("No balance at {}", forward_addr);
            return Ok(());
        }

        info!("Balance at {}:", forward_addr);
        for balance in &balances {
            info!("  {} {}", balance.amount, balance.denom);
        }

        let quoted_fee = self.query_igp_fee(request.dest_domain).await?;
        let quoted_fee_f64: f64 = quoted_fee.parse().context("Failed to parse IGP fee")?;
        let max_fee = (quoted_fee_f64 * self.config.igp_fee_buffer) as u64;
        let max_igp_fee = format!("{}utia", max_fee);

        info!(
            "IGP fee for domain {}: quoted={}, max={} ({}x buffer)",
            request.dest_domain, quoted_fee, max_igp_fee, self.config.igp_fee_buffer
        );

        match self
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

                if let Err(err) = self.complete_request(forward_addr).await {
                    warn!(
                        "Failed to remove backend request for {}: {:#}",
                        forward_addr, err
                    );
                    return Err(err);
                }
            }
            Err(err) => {
                error!(
                    "Failed to submit forwarding for {}: {:#}",
                    forward_addr, err
                );
                return Err(err);
            }
        }

        Ok(())
    }
}
