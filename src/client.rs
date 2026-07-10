use anyhow::{Context, Result};
use celestia_grpc::{GrpcClient, TxConfig as CelestiaTxConfig};
use celestia_proto::cosmos::bank::v1beta1::{
    query_client::QueryClient as BankQueryClient, QueryAllBalancesRequest,
};
use cosmrs::{crypto::secp256k1::SigningKey, AccountId};
use futures::future::BoxFuture;
use metrics::counter;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tonic::transport::{Channel, Endpoint};
use tracing::{info, warn};

use crate::proto::celestia::forwarding::v1::{
    query_client::QueryClient as ForwardingQueryClient, MsgForward, QueryQuoteForwardingFeeRequest,
};
use crate::proto::cosmos::base::v1beta1::Coin;
use crate::Balance;

/// Timeout for gRPC queries (balance, fee).
const GRPC_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Timeout for transaction submission (includes waiting for confirmation).
const TX_SUBMIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Implements the Name trait for Protobuf message type URLs.
impl prost::Name for MsgForward {
    const NAME: &'static str = "MsgForward";
    const PACKAGE: &'static str = "celestia.forwarding.v1";
}

/// One Celestia gRPC endpoint: a lazily-connected channel for queries and a
/// lumina tx client bound to the same URL for submissions.
struct GrpcEndpoint {
    url: String,
    channel: Channel,
    tx_client: GrpcClient,
}

/// Split a comma-separated gRPC URL spec into individual URLs (surrounding
/// whitespace trimmed, empty entries skipped).
fn parse_grpc_urls(spec: &str) -> Vec<String> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Celestia client for balance queries and transaction submission.
///
/// Holds an ordered set of interchangeable gRPC endpoints: the first is the
/// preferred primary and the rest are fallbacks. Queries fail over to the next
/// endpoint within the same call; a failed submission rotates the preferred
/// endpoint so the higher-level backoff retry uses the fallback. The preference
/// is sticky — after a failover the healthy endpoint keeps serving instead of
/// every call re-trying the unhealthy primary first.
pub(crate) struct CelestiaClient {
    endpoints: Vec<GrpcEndpoint>,
    current: AtomicUsize,
    signer_address: AccountId,
}

impl CelestiaClient {
    /// Creates and returns a new CelestiaClient using the provided private key.
    /// `grpc_urls` may be a comma-separated list of equivalent endpoints, e.g.
    /// `http://node-a:9090,http://node-b:9090`; a single URL yields a
    /// one-endpoint pool, i.e. the original no-failover behavior.
    pub(crate) async fn new(grpc_urls: String, private_key_hex: String) -> Result<Self> {
        let (private_key_hex, signer_address) = Self::prepare_private_key(&private_key_hex)?;
        let endpoints = parse_grpc_urls(&grpc_urls)
            .into_iter()
            .map(|url| {
                let endpoint = Endpoint::new(url.clone())
                    .with_context(|| {
                        format!("Invalid CELESTIA_GRPC URL (expected http/https): {url}")
                    })?
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(GRPC_QUERY_TIMEOUT);

                let tx_client = GrpcClient::builder()
                    .url(&url)
                    .private_key_hex(private_key_hex.as_str())
                    .build()
                    .with_context(|| {
                        format!("Failed to initialize Celestia gRPC tx client for {url}")
                    })?;

                Ok(GrpcEndpoint {
                    channel: endpoint.connect_lazy(),
                    url,
                    tx_client,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(
            !endpoints.is_empty(),
            "CELESTIA_GRPC contained no usable URLs"
        );

        Ok(Self {
            endpoints,
            current: AtomicUsize::new(0),
            signer_address,
        })
    }

    /// Parses the private key and returns the normalized string (without 0x prefix)
    /// and the associated bech32 account address.
    fn prepare_private_key(private_key_hex: &str) -> Result<(String, AccountId)> {
        let normalized_private_key_hex = private_key_hex.trim().trim_start_matches("0x");
        let private_key =
            hex::decode(normalized_private_key_hex).context("Invalid private key hex")?;
        let signing_key = SigningKey::from_slice(&private_key)
            .map_err(|e| anyhow::anyhow!("Invalid secp256k1 private key: {}", e))?;
        let signer_address = signing_key
            .public_key()
            .account_id("celestia")
            .map_err(|e| anyhow::anyhow!("Failed to get account ID: {}", e))?;

        Ok((normalized_private_key_hex.to_string(), signer_address))
    }

    /// Number of configured gRPC endpoints.
    pub(crate) fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Comma-separated list of endpoint URLs, for logging.
    pub(crate) fn url_list(&self) -> String {
        self.endpoints
            .iter()
            .map(|e| e.url.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Run `op` against each endpoint, starting from the sticky preferred one and
    /// rotating through the fallbacks until one succeeds. A success on a
    /// non-preferred endpoint makes it the new preference.
    async fn with_failover<'a, T>(
        &'a self,
        what: &str,
        op: impl Fn(&'a GrpcEndpoint) -> BoxFuture<'a, Result<T>>,
    ) -> Result<T> {
        let start = self.current.load(Ordering::Relaxed);
        let count = self.endpoints.len();
        let mut last_err = None;
        for i in 0..count {
            let idx = (start + i) % count;
            let endpoint = &self.endpoints[idx];
            match op(endpoint).await {
                Ok(value) => {
                    if idx != start {
                        self.current.store(idx, Ordering::Relaxed);
                        counter!("relayer_grpc_failover_total").increment(1);
                        warn!("Failed over to gRPC endpoint {}", endpoint.url);
                    }
                    return Ok(value);
                }
                Err(e) => {
                    warn!("{what} failed on gRPC endpoint {}: {e:#}", endpoint.url);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.expect("pool has at least one endpoint"))
            .with_context(|| format!("{what} failed on all gRPC endpoints"))
    }

    /// Query all balances for an address via Cosmos bank gRPC query
    pub(crate) async fn query_balances(&self, address: &str) -> Result<Vec<Balance>> {
        let response = self
            .with_failover("Balance query", |endpoint| {
                Box::pin(async move {
                    let mut client = BankQueryClient::new(endpoint.channel.clone());
                    tokio::time::timeout(
                        GRPC_QUERY_TIMEOUT,
                        client.all_balances(QueryAllBalancesRequest {
                            address: address.to_string(),
                            pagination: None,
                            resolve_denom: false,
                        }),
                    )
                    .await
                    .context("Balance query timed out")?
                    .context("Failed to query balances via gRPC")
                })
            })
            .await?
            .into_inner();

        Ok(response
            .balances
            .into_iter()
            .map(|c| Balance {
                denom: c.denom,
                amount: c.amount,
            })
            .collect())
    }

    /// Query IGP fee quote for a destination domain and token via forwarding module gRPC query.
    /// When `custom_hook_id` is set, the quote is against that post-dispatch hook (e.g. an
    /// alternative IGP) so it matches what MsgForward will charge when routed through it.
    pub(crate) async fn query_igp_fee(
        &self,
        dest_domain: u32,
        token_id: &str,
        custom_hook_id: Option<&str>,
    ) -> Result<String> {
        let custom_hook_id = custom_hook_id.unwrap_or_default().to_string();
        let result = self
            .with_failover("IGP fee query", |endpoint| {
                let custom_hook_id = custom_hook_id.clone();
                Box::pin(async move {
                    let mut client = ForwardingQueryClient::new(endpoint.channel.clone());
                    let response = tokio::time::timeout(
                        GRPC_QUERY_TIMEOUT,
                        client.quote_forwarding_fee(QueryQuoteForwardingFeeRequest {
                            dest_domain,
                            token_id: token_id.to_string(),
                            custom_hook_id,
                        }),
                    )
                    .await
                    .context("IGP fee query timed out")?
                    .context("Failed to query IGP fee via forwarding gRPC query")?;
                    Ok(response
                        .into_inner()
                        .fee
                        .map(|f| f.amount)
                        .unwrap_or_else(|| "0".to_string()))
                })
            })
            .await;

        match result {
            Ok(fee) => Ok(fee),
            Err(err) => {
                warn!(
                    "Failed to query IGP fee for domain {} token {} on all gRPC endpoints: {err:#}",
                    dest_domain, token_id
                );
                Ok("0".to_string())
            }
        }
    }

    /// Returns the configured signer address.
    pub(crate) fn signer_address(&self) -> &AccountId {
        &self.signer_address
    }

    /// Submit a forwarding transaction
    pub(crate) async fn submit_forward(
        &self,
        forward_addr: &str,
        dest_domain: u32,
        dest_recipient: &str,
        token_id: &str,
        max_igp_fee: &str,
        custom_hook_id: Option<&str>,
    ) -> Result<String> {
        info!(
            "Submitting forward: addr={}, domain={}, recipient={}, token_id={}, max_fee={}",
            forward_addr, dest_domain, dest_recipient, token_id, max_igp_fee
        );

        // Parse max_igp_fee (e.g., "1100utia")
        let fee_amount = max_igp_fee
            .trim_end_matches("utia")
            .trim_end_matches("utoken");
        let fee_denom = if max_igp_fee.ends_with("utia") {
            "utia"
        } else {
            "utoken"
        };

        let msg_forward = MsgForward {
            signer: self.signer_address.to_string(),
            forward_addr: forward_addr.to_string(),
            dest_domain,
            dest_recipient: dest_recipient.to_string(),
            token_id: token_id.to_string(),
            max_igp_fee: Some(Coin {
                denom: fee_denom.to_string(),
                amount: fee_amount.to_string(),
            }),
            custom_hook_id: custom_hook_id.unwrap_or_default().to_string(),
            custom_hook_metadata: String::new(),
        };

        // Submissions deliberately do NOT fail over within the call: a timed-out
        // submit may still have been broadcast, so an immediate resubmit on a
        // fallback endpoint would at best waste a sequence-mismatch round-trip.
        // Instead a failure rotates the preferred endpoint, and the higher-level
        // submission backoff (which re-queries the balance first, a no-op if the
        // original tx landed) retries against the fallback.
        let idx = self.current.load(Ordering::Relaxed);
        let endpoint = &self.endpoints[idx];
        let result = tokio::time::timeout(
            TX_SUBMIT_TIMEOUT,
            endpoint
                .tx_client
                .submit_message(msg_forward, CelestiaTxConfig::default()),
        )
        .await
        .context("Transaction submission timed out")
        .and_then(|r| r.context("Failed to submit MsgForward"));

        match result {
            Ok(tx_info) => {
                let tx_hash = tx_info.hash.to_string();
                info!("Transaction broadcast successfully: {}", tx_hash);
                Ok(tx_hash)
            }
            Err(e) => {
                if self.endpoints.len() > 1 {
                    let next = (idx + 1) % self.endpoints.len();
                    self.current.store(next, Ordering::Relaxed);
                    counter!("relayer_grpc_failover_total").increment(1);
                    warn!(
                        "Submission failed on gRPC endpoint {}; next attempt will use {}",
                        endpoint.url, self.endpoints[next].url
                    );
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comma_separated_urls_with_whitespace() {
        assert_eq!(
            parse_grpc_urls(" http://a:9090 , http://b:9090,,http://c:9090 "),
            vec!["http://a:9090", "http://b:9090", "http://c:9090"]
        );
    }

    #[test]
    fn single_url_yields_one_endpoint() {
        assert_eq!(parse_grpc_urls("http://a:9090"), vec!["http://a:9090"]);
    }

    #[test]
    fn empty_spec_yields_no_urls() {
        assert!(parse_grpc_urls(" , ").is_empty());
    }
}
