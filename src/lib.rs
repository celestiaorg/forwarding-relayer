use anyhow::{Context, Result};
use bech32::{Bech32, Hrp};
use clap::Parser;
use cosmrs::{
    crypto::secp256k1::SigningKey,
    tx::{self, Fee, SignDoc, SignerInfo},
    AccountId, Any as CosmosAny, Coin as CosmosCoin,
};
use prost::Message;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::time::Duration;
use tendermint_rpc::{Client as TendermintClient, HttpClient as TendermintHttpClient};
use tracing::{debug, error, info, warn};

/// Forwarding relayer configuration
#[derive(Parser, Debug)]
#[command(author, version, about = "Celestia forwarding relayer", long_about = None)]
pub struct Config {
    /// Celestia RPC URL
    #[arg(long, env = "CELESTIA_RPC", default_value = "http://localhost:26657")]
    pub celestia_rpc: String,

    /// Celestia gRPC URL
    #[arg(long, env = "CELESTIA_GRPC", default_value = "http://localhost:9090")]
    pub celestia_grpc: String,

    /// Destination domain ID (e.g., 42161 for Arbitrum)
    #[arg(long, env = "DEST_DOMAIN")]
    pub dest_domain: u32,

    /// Destination recipient (32-byte hex address with 0x prefix)
    #[arg(long, env = "DEST_RECIPIENT")]
    pub dest_recipient: String,

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
}

/// Token balance
#[derive(Debug, Clone)]
pub struct Balance {
    pub denom: String,
    pub amount: String,
}

/// MsgForward protobuf message for celestia.forwarding.v1
#[derive(Clone, PartialEq, prost::Message)]
struct MsgForward {
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
struct CelestiaClient {
    rpc_url: String,
    #[allow(dead_code)]
    grpc_url: String,
    client: HttpClient,
    tendermint_client: TendermintHttpClient,
    signing_key: SigningKey,
    signer_address: AccountId,
    chain_id: String,
}

impl CelestiaClient {
    async fn new(
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
    async fn query_balance(&self, address: &str) -> Result<Vec<Balance>> {
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
    async fn query_igp_fee(&self, dest_domain: u32) -> Result<String> {
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
            return Ok("1000".to_string());
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
    async fn submit_forward(
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

/// Derive a forwarding address from dest_domain and dest_recipient
///
/// Algorithm from celestia-app/x/forwarding/types/address.go:
/// 1. callDigest = sha256(destDomain_32bytes || destRecipient)
/// 2. salt = sha256(ForwardVersion || callDigest)
/// 3. address = address.Module("forwarding", salt)[:20]
/// 4. Encode as bech32 with "celestia" prefix
///
/// Where address.Module(name, key) is:
///   th = sha256(typ)
///   sha256(th || name || 0x00 || key)
pub fn derive_forwarding_address(dest_domain: u32, dest_recipient: &str) -> Result<String> {
    // Parse dest_recipient as hex (with or without 0x prefix)
    let recipient_hex = dest_recipient.trim_start_matches("0x");
    let recipient_bytes =
        hex::decode(recipient_hex).context("Failed to decode dest_recipient as hex")?;

    if recipient_bytes.len() != 32 {
        anyhow::bail!(
            "dest_recipient must be exactly 32 bytes, got {}",
            recipient_bytes.len()
        );
    }

    // Step 1: Encode dest_domain as 32-byte big-endian (right-aligned at offset 28)
    let mut domain_bytes = [0u8; 32];
    domain_bytes[28..32].copy_from_slice(&dest_domain.to_be_bytes());

    // Step 2: callDigest = sha256(destDomain_32bytes || destRecipient)
    let mut hasher = Sha256::new();
    hasher.update(domain_bytes);
    hasher.update(&recipient_bytes);
    let call_digest = hasher.finalize();

    // Step 3: salt = sha256(ForwardVersion || callDigest)
    const FORWARD_VERSION: u8 = 1;
    let mut hasher = Sha256::new();
    hasher.update([FORWARD_VERSION]);
    hasher.update(call_digest);
    let salt = hasher.finalize();

    // Step 4: address = address.Module("forwarding", salt)
    // address.Module(name, key) computes:
    //   th = sha256("module")
    //   sha256(th || name || 0x00 || key)

    // Compute th = sha256("module")
    let mut hasher = Sha256::new();
    hasher.update(b"module");
    let th = hasher.finalize();

    // Compute final address = sha256(th || "forwarding" || 0x00 || salt)[:20]
    let mut hasher = Sha256::new();
    hasher.update(th);
    hasher.update(b"forwarding");
    hasher.update([0x00]);
    hasher.update(salt);
    let addr_hash = hasher.finalize();
    let addr_bytes = &addr_hash[..20];

    // Encode as bech32
    let hrp = Hrp::parse("celestia").expect("valid hrp");
    let address =
        bech32::encode::<Bech32>(hrp, addr_bytes).context("Failed to encode address as bech32")?;

    Ok(address)
}

/// Relayer state
pub struct Relayer {
    config: Config,
    celestia: CelestiaClient,
    forward_addr: String,
    balance_cache: Vec<Balance>,
}

impl Relayer {
    pub async fn new(config: Config) -> Result<Self> {
        // For tendermint RPC, use port 26657 by default
        let tendermint_rpc_url = config.celestia_rpc.replace(":1317", ":26657");

        let celestia = CelestiaClient::new(
            config.celestia_rpc.clone(),
            tendermint_rpc_url,
            config.celestia_grpc.clone(),
            config.relayer_mnemonic.clone(),
            config.chain_id.clone(),
        )
        .await?;

        // Derive the forwarding address from config
        let forward_addr = derive_forwarding_address(config.dest_domain, &config.dest_recipient)?;

        info!("Relayer address: {}", celestia.signer_address);

        Ok(Self {
            config,
            celestia,
            forward_addr,
            balance_cache: Vec::new(),
        })
    }

    /// Main relayer loop
    pub async fn run(&mut self) -> Result<()> {
        info!("Starting forwarding relayer");
        info!("Celestia RPC: {}", self.config.celestia_rpc);
        info!("Destination Domain: {}", self.config.dest_domain);
        info!("Destination Recipient: {}", self.config.dest_recipient);
        info!("Forwarding Address: {}", self.forward_addr);
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
        debug!("Checking balance at {}", self.forward_addr);

        // Query current balance
        let balances = self.celestia.query_balance(&self.forward_addr).await?;

        // Check if balance has changed (gone up)
        let balance_increased = !balances.is_empty()
            && (self.balance_cache.is_empty() || !balances_equal(&self.balance_cache, &balances));

        if !balance_increased {
            debug!("No new deposits detected");
            return Ok(());
        }

        if balances.is_empty() {
            debug!("No balance at forwarding address");
            self.balance_cache = balances;
            return Ok(());
        }

        info!("New deposit detected! Balance changed:");
        for balance in &balances {
            info!("  {} {}", balance.amount, balance.denom);
        }

        // Query IGP fee and apply buffer
        let quoted_fee = self.celestia.query_igp_fee(self.config.dest_domain).await?;
        let quoted_fee_f64: f64 = quoted_fee.parse().context("Failed to parse IGP fee")?;
        let max_fee = (quoted_fee_f64 * self.config.igp_fee_buffer) as u64;
        let max_igp_fee = format!("{}utia", max_fee);

        info!(
            "IGP fee: quoted={}, max={} ({}x buffer)",
            quoted_fee, max_igp_fee, self.config.igp_fee_buffer
        );

        // Submit forwarding transaction
        match self
            .celestia
            .submit_forward(
                &self.forward_addr,
                self.config.dest_domain,
                &self.config.dest_recipient,
                &max_igp_fee,
            )
            .await
        {
            Ok(tx_hash) => {
                info!("Forwarding submitted: tx_hash={}", tx_hash);

                // Update balance cache
                let new_balance = self.celestia.query_balance(&self.forward_addr).await?;
                self.balance_cache = new_balance;
            }
            Err(e) => {
                error!("Failed to submit forwarding: {:#}", e);
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
