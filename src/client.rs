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
use tonic::metadata::AsciiMetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};
use tracing::{info, warn};

use crate::proto::celestia::forwarding::v1::{
    query_client::QueryClient as ForwardingQueryClient, MsgForward, QueryQuoteForwardingFeeRequest,
};
use crate::proto::cosmos::base::v1beta1::Coin;
use crate::{parse_endpoint_specs, AuthToken, Balance, EndpointSpec};

/// Timeout for gRPC queries (balance, fee).
const GRPC_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Timeout for transaction submission (includes waiting for confirmation).
const TX_SUBMIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Implements the Name trait for Protobuf message type URLs.
impl prost::Name for MsgForward {
    const NAME: &'static str = "MsgForward";
    const PACKAGE: &'static str = "celestia.forwarding.v1";
}

/// gRPC metadata key carrying the optional auth token, e.g. for a
/// token-gated gRPC gateway in front of the Celestia node.
const AUTH_METADATA_KEY: &str = "x-token";

/// tonic interceptor that attaches the optional `x-token` auth metadata to every
/// request made over a query channel. A `None` token is a no-op, preserving the
/// original unauthenticated behavior.
#[derive(Clone)]
struct AuthInterceptor {
    token: Option<AsciiMetadataValue>,
}

impl Interceptor for AuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        if let Some(token) = &self.token {
            request
                .metadata_mut()
                .insert(AUTH_METADATA_KEY, token.clone());
        }
        Ok(request)
    }
}

/// A query channel with the `x-token` auth interceptor baked in. Query clients are
/// built from this via `QueryClient::new`, so a query can never be issued on an
/// unauthenticated channel — there is no bare `Channel` to accidentally reach for.
type AuthenticatedChannel = InterceptedService<Channel, AuthInterceptor>;

/// One Celestia gRPC endpoint: a lazily-connected authenticated channel for queries
/// and a lumina tx client bound to the same URL for submissions.
struct GrpcEndpoint {
    url: String,
    channel: AuthenticatedChannel,
    tx_client: GrpcClient,
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
    /// `grpc_urls` is a comma-separated list of `url|token` (or bare `url`)
    /// endpoints, e.g. `http://node-a:9090|tokA,http://node-b:9090`; the first is
    /// the preferred primary and the rest are fallbacks.
    ///
    /// Each endpoint's optional token is sent as `x-token` gRPC metadata on every
    /// request to *that* endpoint (both queries and submissions) — for a token-gated
    /// gateway. An endpoint with no token stays unauthenticated.
    pub(crate) async fn new(grpc_urls: String, private_key_hex: String) -> Result<Self> {
        let (private_key_hex, signer_address) = Self::prepare_private_key(&private_key_hex)?;
        let endpoints = parse_endpoint_specs(&grpc_urls)?
            .into_iter()
            .map(|EndpointSpec { url, token }| {
                let endpoint = Endpoint::new(url.clone())
                    .with_context(|| {
                        format!("Invalid CELESTIA_GRPC URL (expected http/https): {url}")
                    })?
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(GRPC_QUERY_TIMEOUT);

                // lumina tx endpoint: this endpoint's own `x-token` metadata.
                let mut tx_endpoint = celestia_grpc::Endpoint::new(url.clone());
                if let Some(token) = &token {
                    tx_endpoint = tx_endpoint.metadata(AUTH_METADATA_KEY, token.as_str());
                }
                let tx_client = GrpcClient::builder()
                    .url(tx_endpoint)
                    .private_key_hex(private_key_hex.as_str())
                    .build()
                    .with_context(|| {
                        format!("Failed to initialize Celestia gRPC tx client for {url}")
                    })?;

                // Bake this endpoint's interceptor into the channel so query clients
                // built from it always carry the right token.
                let channel = InterceptedService::new(
                    endpoint.connect_lazy(),
                    AuthInterceptor {
                        token: token.as_ref().map(AuthToken::metadata_value),
                    },
                );

                Ok(GrpcEndpoint {
                    channel,
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

    /// Query IGP fee quote for a destination domain and token via forwarding module gRPC query
    pub(crate) async fn query_igp_fee(&self, dest_domain: u32, token_id: &str) -> Result<String> {
        let result = self
            .with_failover("IGP fee query", |endpoint| {
                Box::pin(async move {
                    let mut client = ForwardingQueryClient::new(endpoint.channel.clone());
                    let response = tokio::time::timeout(
                        GRPC_QUERY_TIMEOUT,
                        client.quote_forwarding_fee(QueryQuoteForwardingFeeRequest {
                            dest_domain,
                            token_id: token_id.to_string(),
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
    fn interceptor_inserts_x_token_when_set() {
        let mut interceptor = AuthInterceptor {
            token: Some(AsciiMetadataValue::from_static("secret-token")),
        };
        let request = interceptor.call(Request::new(())).unwrap();
        assert_eq!(
            request
                .metadata()
                .get(AUTH_METADATA_KEY)
                .and_then(|v| v.to_str().ok()),
            Some("secret-token")
        );
    }

    #[test]
    fn interceptor_is_noop_when_unset() {
        let mut interceptor = AuthInterceptor { token: None };
        let request = interceptor.call(Request::new(())).unwrap();
        assert!(request.metadata().get(AUTH_METADATA_KEY).is_none());
    }

    // A valid secp256k1 key (scalar = 1) for constructing a client offline; the
    // channel connects lazily so no network is touched.
    const TEST_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    #[tokio::test]
    async fn builds_without_auth_token() {
        let client = CelestiaClient::new("http://localhost:9090".to_string(), TEST_KEY.to_string())
            .await
            .expect("client must build with no auth token (unauthenticated node)");
        assert_eq!(client.endpoint_count(), 1);
    }

    #[tokio::test]
    async fn builds_with_per_endpoint_auth_tokens() {
        let client = CelestiaClient::new(
            "http://a:9090|tokA,http://b:9090|tokB".to_string(),
            TEST_KEY.to_string(),
        )
        .await
        .expect("client must build with per-endpoint auth tokens");
        assert_eq!(client.endpoint_count(), 2);
    }
}
