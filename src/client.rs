use anyhow::{Context, Result};
use celestia_grpc::{GrpcClient, TxConfig as CelestiaTxConfig};
use celestia_proto::cosmos::bank::v1beta1::{
    query_client::QueryClient as BankQueryClient, QueryAllBalancesRequest,
};
use cosmrs::{crypto::secp256k1::SigningKey, AccountId};
use tonic::transport::{Channel, Endpoint};
use tracing::{info, warn};

use crate::proto::celestia::forwarding::v1::{
    query_client::QueryClient as ForwardingQueryClient, MsgForward, QueryQuoteForwardingFeeRequest,
};
use crate::proto::cosmos::base::v1beta1::Coin;
use crate::Balance;

impl prost::Name for MsgForward {
    const NAME: &'static str = "MsgForward";
    const PACKAGE: &'static str = "celestia.forwarding.v1";
}

/// Celestia client for balance queries and transaction submission
pub(crate) struct CelestiaClient {
    channel: Channel,
    tx_client: GrpcClient,
    pub(crate) signer_address: AccountId,
}

impl CelestiaClient {
    pub(crate) async fn new(grpc_url: String, private_key_hex: String) -> Result<Self> {
        let private_key_hex = private_key_hex.trim().trim_start_matches("0x").to_string();
        let private_key = hex::decode(&private_key_hex).context("Invalid private key hex")?;
        let signing_key = SigningKey::from_slice(&private_key)
            .map_err(|e| anyhow::anyhow!("Invalid secp256k1 private key: {}", e))?;
        let signer_address = signing_key
            .public_key()
            .account_id("celestia")
            .map_err(|e| anyhow::anyhow!("Failed to get account ID: {}", e))?;

        let endpoint = Endpoint::from_shared(grpc_url.clone()).with_context(|| {
            format!(
                "Invalid CELESTIA_GRPC URL (expected http/https): {}",
                grpc_url
            )
        })?;
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

    /// Query all balances for an address via Cosmos bank gRPC query
    pub(crate) async fn query_balances(&self, address: &str) -> Result<Vec<Balance>> {
        let mut client = BankQueryClient::new(self.channel.clone());
        let response = client
            .all_balances(QueryAllBalancesRequest {
                address: address.to_string(),
                pagination: None,
                resolve_denom: false,
            })
            .await
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
        let response = client
            .quote_forwarding_fee(QueryQuoteForwardingFeeRequest { dest_domain })
            .await;

        match response {
            Ok(resp) => Ok(resp
                .into_inner()
                .fee
                .map(|f| f.amount)
                .unwrap_or_else(|| "0".to_string())),
            Err(err) => {
                warn!(
                    "Failed to query IGP fee for domain {} via forwarding gRPC query: {}",
                    dest_domain, err
                );
                // Return a default fee of 0 if query fails
                Ok("0".to_string())
            }
        }
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

        let tx_info = self
            .tx_client
            .submit_message(msg_forward, CelestiaTxConfig::default())
            .await
            .context("Failed to submit MsgForward")?;

        let tx_hash = tx_info.hash.to_string();
        info!("Transaction broadcast successfully: {}", tx_hash);
        Ok(tx_hash)
    }
}
