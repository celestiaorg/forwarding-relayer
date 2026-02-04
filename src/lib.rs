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
