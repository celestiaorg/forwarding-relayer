use anyhow::{Context, Result};
use celestia_grpc::{GrpcClient, TxConfig as CelestiaTxConfig};
use celestia_proto::cosmos::bank::v1beta1::{
    query_client::QueryClient as BankQueryClient, QueryAllBalancesRequest,
};
use cosmrs::{crypto::secp256k1::SigningKey, AccountId};
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

/// Celestia client for balance queries and transaction submission
pub(crate) struct CelestiaClient {
    channel: Channel,
    tx_client: GrpcClient,
    signer_address: AccountId,
}

impl CelestiaClient {
    /// Creates and returns a new CelestiaClient using the provided private key.
    pub(crate) async fn new(grpc_url: String, private_key_hex: String) -> Result<Self> {
        let (private_key_hex, signer_address) = Self::prepare_private_key(&private_key_hex)?;
        let endpoint = Endpoint::from_shared(grpc_url.clone())
            .with_context(|| {
                format!(
                    "Invalid CELESTIA_GRPC URL (expected http/https): {}",
                    grpc_url
                )
            })?
            .connect_timeout(Duration::from_secs(10))
            .timeout(GRPC_QUERY_TIMEOUT);

        let channel = endpoint.connect_lazy();

        let tx_client = GrpcClient::builder()
            .transport(channel.clone())
            .private_key_hex(private_key_hex.as_str())
            .build()
            .context("Failed to initialize Celestia gRPC tx client")?;

        Ok(Self {
            channel,
            tx_client,
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

    /// Query all balances for an address via Cosmos bank gRPC query
    pub(crate) async fn query_balances(&self, address: &str) -> Result<Vec<Balance>> {
        let mut client = BankQueryClient::new(self.channel.clone());
        let response = tokio::time::timeout(
            GRPC_QUERY_TIMEOUT,
            client.all_balances(QueryAllBalancesRequest {
                address: address.to_string(),
                pagination: None,
                resolve_denom: false,
            }),
        )
        .await
        .context("Balance query timed out")?
        .context("Failed to query balances via gRPC")?
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

    /// Query IGP fee quote for a destination domain via forwarding module gRPC query
    pub(crate) async fn query_igp_fee(&self, dest_domain: u32) -> Result<String> {
        let mut client = ForwardingQueryClient::new(self.channel.clone());
        let response = tokio::time::timeout(
            GRPC_QUERY_TIMEOUT,
            client.quote_forwarding_fee(QueryQuoteForwardingFeeRequest { dest_domain }),
        )
        .await;

        match response {
            Ok(Ok(resp)) => Ok(resp
                .into_inner()
                .fee
                .map(|f| f.amount)
                .unwrap_or_else(|| "0".to_string())),
            Ok(Err(err)) => {
                warn!(
                    "Failed to query IGP fee for domain {} via forwarding gRPC query: {}",
                    dest_domain, err
                );
                Ok("0".to_string())
            }
            Err(_) => {
                warn!(
                    "IGP fee query timed out for domain {}",
                    dest_domain
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
        max_igp_fee: &str,
    ) -> Result<String> {
        info!(
            "Submitting forward: addr={}, domain={}, recipient={}, max_fee={}",
            forward_addr, dest_domain, dest_recipient, max_igp_fee
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
            max_igp_fee: Some(Coin {
                denom: fee_denom.to_string(),
                amount: fee_amount.to_string(),
            }),
        };

        let tx_info = tokio::time::timeout(
            TX_SUBMIT_TIMEOUT,
            self.tx_client
                .submit_message(msg_forward, CelestiaTxConfig::default()),
        )
        .await
        .context("Transaction submission timed out")?
        .context("Failed to submit MsgForward")?;

        let tx_hash = tx_info.hash.to_string();
        info!("Transaction broadcast successfully: {}", tx_hash);
        Ok(tx_hash)
    }
}
