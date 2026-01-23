use anyhow::{Context, Result};
use cosmrs::{
    crypto::secp256k1::SigningKey,
    tx::{self, Fee, SignDoc, SignerInfo},
    AccountId, Any as CosmosAny, Coin as CosmosCoin,
};
use prost::Message;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use tendermint_rpc::{Client as TendermintClient, HttpClient as TendermintHttpClient};
use tracing::{info, warn};

use crate::Balance;

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
    rpc_url: String,
    #[allow(dead_code)]
    grpc_url: String,
    client: HttpClient,
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
        mnemonic: String,
        chain_id: String,
    ) -> Result<Self> {
        // Derive signing key from mnemonic using BIP44 derivation path for Cosmos
        // m/44'/118'/0'/0/0
        let seed = bip39::Mnemonic::parse(&mnemonic)?.to_seed("");
        let path = "m/44'/118'/0'/0/0".parse()?;
        let signing_key = SigningKey::derive_from_path(seed, &path)?;
        let signer_address = signing_key
            .public_key()
            .account_id("celestia")
            .map_err(|e| anyhow::anyhow!("Failed to get account ID: {}", e))?;

        let tendermint_client = TendermintHttpClient::new(tendermint_rpc_url.as_str())?;

        Ok(Self {
            rpc_url,
            grpc_url,
            client: HttpClient::new(),
            tendermint_client,
            signing_key,
            signer_address,
            chain_id,
        })
    }

    /// Query balance at an address using RPC
    pub(crate) async fn query_balance(&self, address: &str) -> Result<Vec<Balance>> {
        let url = format!("{}/cosmos/bank/v1beta1/balances/{}", self.rpc_url, address);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to query balance")?;

        if !response.status().is_success() {
            anyhow::bail!("RPC returned error: {}", response.status());
        }

        #[derive(Deserialize)]
        struct BalanceResponse {
            balances: Vec<CoinResponse>,
        }

        #[derive(Deserialize)]
        struct CoinResponse {
            denom: String,
            amount: String,
        }

        let resp = response
            .json::<BalanceResponse>()
            .await
            .context("Failed to parse balance response")?;

        Ok(resp
            .balances
            .into_iter()
            .map(|c| Balance {
                denom: c.denom,
                amount: c.amount,
            })
            .collect())
    }

    /// Query IGP fee quote for a destination domain
    pub(crate) async fn query_igp_fee(&self, dest_domain: u32) -> Result<String> {
        let url = format!(
            "{}/celestia/forwarding/v1/quote_fee/{}",
            self.rpc_url, dest_domain
        );

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to query IGP fee")?;

        if !response.status().is_success() {
            warn!(
                "Failed to query IGP fee for domain {}: {}",
                dest_domain,
                response.status()
            );
            // Return a default fee if query fails
            return Ok("0".to_string());
        }

        #[derive(Deserialize)]
        struct FeeResponse {
            fee: CoinResponse,
        }

        #[derive(Deserialize)]
        struct CoinResponse {
            amount: String,
        }

        let resp = response
            .json::<FeeResponse>()
            .await
            .context("Failed to parse fee response")?;

        Ok(resp.fee.amount)
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

    /// Query account number and sequence
    async fn query_account(&self, address: &str) -> Result<AccountInfo> {
        let url = format!("{}/cosmos/auth/v1beta1/accounts/{}", self.rpc_url, address);

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to query account")?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to query account: {}", response.status());
        }

        #[derive(Deserialize)]
        struct AccountResponse {
            account: Account,
        }

        #[derive(Deserialize)]
        struct Account {
            account_number: String,
            sequence: String,
        }

        let resp = response
            .json::<AccountResponse>()
            .await
            .context("Failed to parse account response")?;

        Ok(AccountInfo {
            account_number: resp.account.account_number.parse()?,
            sequence: resp.account.sequence.parse()?,
        })
    }

    /// Broadcast a signed transaction
    async fn broadcast_tx(&self, tx_bytes: Vec<u8>) -> Result<String> {
        let response = self
            .tendermint_client
            .broadcast_tx_sync(tx_bytes)
            .await
            .context("Failed to broadcast transaction")?;

        if response.code.is_err() {
            anyhow::bail!(
                "Transaction failed: code={:?}, log={}",
                response.code,
                response.log
            );
        }

        Ok(response.hash.to_string())
    }
}

#[derive(Debug)]
struct AccountInfo {
    account_number: u64,
    sequence: u64,
}
