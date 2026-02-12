use anyhow::{Context, Result};
use bech32::{Bech32, Hrp};
use clap::Parser;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod backend;
mod client;
mod relayer;

// Re-export public types from modules
pub use backend::{Backend, BackendConfig, BackendState};
pub use relayer::{balances_equal, Relayer, RelayerConfig};

/// Forwarding relayer CLI
#[derive(Parser, Debug)]
#[command(author, version, about = "Celestia forwarding relayer", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// CLI commands
#[derive(Parser, Debug)]
pub enum Command {
    /// Run the relayer
    Relayer(RelayerConfig),
    /// Run the backend server
    Backend(BackendConfig),
    /// Derive a forwarding address
    DeriveAddress {
        /// Destination domain (e.g., 1234 for Anvil)
        #[arg(long)]
        dest_domain: u32,
        /// Destination recipient address (32-byte hex, e.g., 0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266)
        #[arg(long)]
        dest_recipient: String,
    },
    /// Derive a private key from a mnemonic
    DerivePrivateKey {
        /// BIP39 mnemonic phrase
        #[arg(long)]
        mnemonic: String,
    },
}

/// Forwarding request from backend API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardingRequest {
    pub id: String,
    pub forward_addr: String,
    pub dest_domain: u32,
    pub dest_recipient: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Request body for creating a new forwarding request (no ID - server generates it)
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateForwardingRequest {
    pub forward_addr: String,
    pub dest_domain: u32,
    pub dest_recipient: String,
}

/// Status update request
#[derive(Debug, Serialize, Deserialize)]
pub struct StatusUpdate {
    pub status: String,
}

/// Token balance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balance {
    pub denom: String,
    pub amount: String,
}

/// Derive a Celestia account address from a secp256k1 private key hex string.
pub fn derive_relayer_address_from_private_key_hex(private_key_hex: &str) -> Result<String> {
    use cosmrs::crypto::secp256k1::SigningKey;

    let private_key_hex = private_key_hex.trim().trim_start_matches("0x");
    let private_key = hex::decode(private_key_hex).context("Invalid private key hex")?;
    let signing_key = SigningKey::from_slice(&private_key)
        .map_err(|e| anyhow::anyhow!("Invalid secp256k1 private key: {}", e))?;
    let address = signing_key
        .public_key()
        .account_id("celestia")
        .map_err(|e| anyhow::anyhow!("Failed to derive address: {}", e))?;
    Ok(address.to_string())
}

/// Derive a secp256k1 private key from a BIP39 mnemonic using standard Cosmos derivation path.
/// Returns the private key as a hex string.
pub fn derive_private_key_from_mnemonic(mnemonic: &str) -> Result<String> {
    use cosmrs::bip32::{DerivationPath, XPrv};

    // Parse the mnemonic and convert to seed
    let mnemonic_parsed = bip39::Mnemonic::parse(mnemonic)
        .map_err(|e| anyhow::anyhow!("Invalid mnemonic: {}", e))?;
    let seed = mnemonic_parsed.to_seed("");

    // Standard Cosmos HD path: m/44'/118'/0'/0/0
    let path: DerivationPath = "m/44'/118'/0'/0/0"
        .parse()
        .context("Failed to parse derivation path")?;

    // Derive the extended private key
    let xprv = XPrv::derive_from_path(&seed, &path).context("Failed to derive key from path")?;

    // Get the raw private key bytes (32 bytes)
    let private_key_bytes = xprv.to_bytes();

    Ok(hex::encode(&private_key_bytes))
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
