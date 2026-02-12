use anyhow::{Context, Result};
use cosmrs::{
    crypto::secp256k1::SigningKey,
    tx::{self, Fee, SignDoc, SignerInfo},
    AccountId, Any as CosmosAny, Coin as CosmosCoin,
};
use prost::Message;
use tendermint_rpc::{Client as TendermintClient, HttpClient as TendermintHttpClient};
use tracing::{info, warn};

use crate::Balance;

/// Protobuf types for ABCI queries
mod query_proto {
    /// QueryAllBalancesRequest for bank module
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct QueryAllBalancesRequest {
        #[prost(string, tag = "1")]
        pub address: String,
    }

    /// QueryAllBalancesResponse from bank module
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct QueryAllBalancesResponse {
        #[prost(message, repeated, tag = "1")]
        pub balances: Vec<Coin>,
    }

    /// Coin type
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Coin {
        #[prost(string, tag = "1")]
        pub denom: String,
        #[prost(string, tag = "2")]
        pub amount: String,
    }

    /// QueryAccountRequest for auth module
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct QueryAccountRequest {
        #[prost(string, tag = "1")]
        pub address: String,
    }

    /// QueryAccountResponse from auth module
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct QueryAccountResponse {
        #[prost(message, optional, tag = "1")]
        pub account: Option<prost_types::Any>,
    }

    /// BaseAccount type
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct BaseAccount {
        #[prost(string, tag = "1")]
        pub address: String,
        #[prost(message, optional, tag = "2")]
        pub pub_key: Option<prost_types::Any>,
        #[prost(uint64, tag = "3")]
        pub account_number: u64,
        #[prost(uint64, tag = "4")]
        pub sequence: u64,
    }
}

/// MsgForward protobuf message for celestia.forwarding.v1
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MsgForward {
    #[prost(string, tag = "1")]
    pub signer: String,
    #[prost(string, tag = "2")]
    pub forward_addr: String,
    #[prost(uint32, tag = "3")]
    pub dest_domain: u32,
    #[prost(string, tag = "4")]
    pub dest_recipient: String,
    #[prost(message, required, tag = "5")]
    pub max_igp_fee: cosmos_sdk_proto::cosmos::base::v1beta1::Coin,
}

impl MsgForward {
    fn to_any(&self) -> Result<CosmosAny> {
        let mut buf = Vec::new();
        Message::encode(self, &mut buf)?;

        Ok(CosmosAny {
            type_url: "/celestia.forwarding.v1.MsgForward".to_string(),
            value: buf,
        })
    }
}

/// Celestia client for balance queries and transaction submission
pub(crate) struct CelestiaClient {
    #[allow(dead_code)]
    rpc_url: String,
    #[allow(dead_code)]
    grpc_url: String,
    tendermint_client: TendermintHttpClient,
    signing_key: SigningKey,
    pub(crate) signer_address: AccountId,
    chain_id: String,
}

impl CelestiaClient {
    pub(crate) async fn new(
        rpc_url: String,
        tendermint_rpc_url: String,
        grpc_url: String,
        private_key_hex: String,
        chain_id: String,
    ) -> Result<Self> {
        let private_key_hex = private_key_hex.trim().trim_start_matches("0x");
        let private_key = hex::decode(private_key_hex).context("Invalid relayer private key hex")?;
        anyhow::ensure!(
            private_key.len() == 32,
            "Relayer private key must be 32 bytes, got {} bytes",
            private_key.len()
        );
        let signing_key = SigningKey::from_slice(&private_key)
            .map_err(|e| anyhow::anyhow!("Invalid secp256k1 private key: {}", e))?;
        let signer_address = signing_key
            .public_key()
            .account_id("celestia")
            .map_err(|e| anyhow::anyhow!("Failed to get account ID: {}", e))?;

        let tendermint_client = TendermintHttpClient::new(tendermint_rpc_url.as_str())?;

        Ok(Self {
            rpc_url,
            grpc_url,
            tendermint_client,
            signing_key,
            signer_address,
            chain_id,
        })
    }

    /// Query balance at an address using Tendermint RPC ABCI query
    pub(crate) async fn query_balances(&self, address: &str) -> Result<Vec<Balance>> {
        // Build the query request
        let request = query_proto::QueryAllBalancesRequest {
            address: address.to_string(),
        };

        let mut request_bytes = Vec::new();
        Message::encode(&request, &mut request_bytes)?;

        // Query via ABCI
        let response = self
            .tendermint_client
            .abci_query(
                Some("/cosmos.bank.v1beta1.Query/AllBalances".to_string()),
                request_bytes,
                None,
                false,
            )
            .await
            .context("Failed to query balances via ABCI")?;

        if response.code.is_err() {
            anyhow::bail!(
                "ABCI query failed: code={:?}, log={}",
                response.code,
                response.log
            );
        }

        // Decode the response
        let balance_response: query_proto::QueryAllBalancesResponse =
            Message::decode(response.value.as_slice())
                .context("Failed to decode balance response")?;

        Ok(balance_response
            .balances
            .into_iter()
            .map(|c| Balance {
                denom: c.denom,
                amount: c.amount,
            })
            .collect())
    }

    /// Query IGP fee quote for a destination domain using Tendermint RPC ABCI query
    pub(crate) async fn query_igp_fee(&self, dest_domain: u32) -> Result<String> {
        // Build the query request for the forwarding module
        // The protobuf message for QuoteFeeRequest
        #[derive(Clone, PartialEq, prost::Message)]
        struct QuoteFeeRequest {
            #[prost(uint32, tag = "1")]
            dest_domain: u32,
        }

        #[derive(Clone, PartialEq, prost::Message)]
        struct QuoteFeeResponse {
            #[prost(message, optional, tag = "1")]
            fee: Option<query_proto::Coin>,
        }

        let request = QuoteFeeRequest { dest_domain };

        let mut request_bytes = Vec::new();
        Message::encode(&request, &mut request_bytes)?;

        // Query via ABCI
        let response = self
            .tendermint_client
            .abci_query(
                Some("/celestia.forwarding.v1.Query/QuoteForwardingFee".to_string()),
                request_bytes,
                None,
                false,
            )
            .await
            .context("Failed to query IGP fee via ABCI")?;

        if response.code.is_err() {
            warn!(
                "Failed to query IGP fee for domain {}: code={:?}, log={}",
                dest_domain, response.code, response.log
            );
            // Return a default fee of 0 if query fails
            return Ok("0".to_string());
        }

        // Decode the response
        let fee_response: QuoteFeeResponse =
            Message::decode(response.value.as_slice()).context("Failed to decode fee response")?;

        Ok(fee_response
            .fee
            .map(|f| f.amount)
            .unwrap_or_else(|| "0".to_string()))
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

        // Query account info for sequence number
        let account_info = self.query_account(self.signer_address.as_ref()).await?;

        // Parse max_igp_fee (e.g., "1100utia")
        let fee_amount = max_igp_fee
            .trim_end_matches("utia")
            .trim_end_matches("utoken");
        let fee_denom = if max_igp_fee.ends_with("utia") {
            "utia"
        } else {
            "utoken"
        };

        // Create MsgForward
        let msg_forward = MsgForward {
            signer: self.signer_address.to_string(),
            forward_addr: forward_addr.to_string(),
            dest_domain,
            dest_recipient: dest_recipient.to_string(),
            max_igp_fee: cosmos_sdk_proto::cosmos::base::v1beta1::Coin {
                denom: fee_denom.to_string(),
                amount: fee_amount.to_string(),
            },
        };

        // Convert to Any
        let msg_any = msg_forward.to_any()?;

        // Create transaction fee (gas fee, not IGP fee)
        let gas_fee = CosmosCoin {
            denom: "utia"
                .parse()
                .map_err(|e| anyhow::anyhow!("Failed to parse denom: {}", e))?,
            amount: 1000u128, // 1000 utia gas fee
        };

        let fee = Fee::from_amount_and_gas(gas_fee, 200000u64);

        // Create transaction body
        let tx_body = tx::BodyBuilder::new().msg(msg_any).finish();

        // Get auth info with signer info
        let signer_info =
            SignerInfo::single_direct(Some(self.signing_key.public_key()), account_info.sequence);
        let auth_info = signer_info.auth_info(fee);

        // Create sign doc
        let chain_id = cosmrs::tendermint::chain::Id::try_from(self.chain_id.as_str())
            .map_err(|e| anyhow::anyhow!("Failed to parse chain ID: {}", e))?;

        let sign_doc = SignDoc::new(&tx_body, &auth_info, &chain_id, account_info.account_number)
            .map_err(|e| anyhow::anyhow!("Failed to create SignDoc: {}", e))?;

        // Sign the transaction
        let tx_raw = sign_doc
            .sign(&self.signing_key)
            .map_err(|e| anyhow::anyhow!("Failed to sign transaction: {}", e))?;

        // Broadcast transaction
        let tx_bytes = tx_raw
            .to_bytes()
            .map_err(|e| anyhow::anyhow!("Failed to serialize transaction: {}", e))?;
        let tx_hash = self.broadcast_tx(tx_bytes).await?;

        info!("Transaction broadcast successfully: {}", tx_hash);

        Ok(tx_hash)
    }

    /// Query account number and sequence using Tendermint RPC ABCI query
    async fn query_account(&self, address: &str) -> Result<AccountInfo> {
        // Build the query request
        let request = query_proto::QueryAccountRequest {
            address: address.to_string(),
        };

        let mut request_bytes = Vec::new();
        Message::encode(&request, &mut request_bytes)?;

        // Query via ABCI
        let response = self
            .tendermint_client
            .abci_query(
                Some("/cosmos.auth.v1beta1.Query/Account".to_string()),
                request_bytes,
                None,
                false,
            )
            .await
            .context("Failed to query account via ABCI")?;

        if response.code.is_err() {
            anyhow::bail!(
                "ABCI query failed: code={:?}, log={}",
                response.code,
                response.log
            );
        }

        // Decode the response
        let account_response: query_proto::QueryAccountResponse =
            Message::decode(response.value.as_slice())
                .context("Failed to decode account response")?;

        let account_any = account_response
            .account
            .ok_or_else(|| anyhow::anyhow!("Account not found"))?;

        // The account is wrapped in an Any type, decode the BaseAccount
        let base_account: query_proto::BaseAccount = Message::decode(account_any.value.as_slice())
            .context("Failed to decode BaseAccount")?;

        Ok(AccountInfo {
            account_number: base_account.account_number,
            sequence: base_account.sequence,
        })
    }

    /// Broadcast a signed transaction
    async fn broadcast_tx(&self, tx_bytes: Vec<u8>) -> Result<String> {
        let response = self
            .tendermint_client
            .broadcast_tx_commit(tx_bytes)
            .await
            .context("Failed to broadcast transaction")?;

        // Check if transaction was included in a block (check_tx)
        if response.check_tx.code.is_err() {
            anyhow::bail!(
                "Transaction rejected by mempool: code={:?}, log={}",
                response.check_tx.code,
                response.check_tx.log
            );
        }

        // Check if transaction executed successfully (deliver_tx)
        if response.tx_result.code.is_err() {
            anyhow::bail!(
                "Transaction execution failed: code={:?}, log={}",
                response.tx_result.code,
                response.tx_result.log
            );
        }

        info!(
            "Transaction executed successfully in block {}",
            response.height
        );
        Ok(response.hash.to_string())
    }
}

#[derive(Debug)]
struct AccountInfo {
    account_number: u64,
    sequence: u64,
}
