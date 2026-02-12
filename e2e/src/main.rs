use anyhow::{Context, Result};
use clap::Parser;
use ethers::prelude::*;
use ethers::types::transaction::eip2718::TypedTransaction;
use std::time::Duration;
use tracing::{info, warn};

use forwarding_relayer::{CreateForwardingRequest, ForwardingRequest};

const DEFAULT_RECIPIENT: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const WARP_ROUTE_CONFIG_PATH: &str =
    "hyperlane/registry/deployments/warp_routes/TIA/warp-config-config.yaml";

#[derive(Parser, Debug)]
#[command(name = "e2e")]
#[command(about = "End-to-end test for forwarding relayer with Anvil")]
struct Args {
    /// Anvil RPC URL
    #[arg(long, default_value = "http://localhost:8545")]
    anvil_rpc: String,

    /// Warp token address on Anvil (auto-detected from deployment files if not provided)
    #[arg(long, env = "WARP_TOKEN")]
    warp_token: Option<String>,

    /// Recipient address on Anvil (20-byte hex)
    #[arg(long, default_value = DEFAULT_RECIPIENT)]
    recipient: String,

    /// Backend port
    #[arg(long, default_value = "8080")]
    backend_port: u16,

    /// Amount to send to forwarding address (in utia)
    #[arg(long, default_value = "1000000")]
    fund_amount: u64,

    /// Timeout for waiting for balance change (seconds)
    #[arg(long, default_value = "120")]
    timeout_secs: u64,

    /// Destination Hyperlane domain
    #[arg(long, default_value = "1234")]
    dest_domain: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let backend_url = format!("http://localhost:{}", args.backend_port);

    info!("=== Forwarding Relayer E2E Test ===");
    info!("Anvil RPC: {}", args.anvil_rpc);
    info!("Backend URL: {}", backend_url);
    info!("Recipient: {}", args.recipient);

    // Auto-detect warp token address from deployment files
    let warp_token = match args.warp_token.clone() {
        Some(t) => t,
        None => detect_warp_token().context(
            "Failed to auto-detect warp token. Provide --warp-token or set WARP_TOKEN env var",
        )?,
    };
    info!("Warp token: {}", warp_token);

    // Compute 32-byte padded recipient for Hyperlane
    let dest_recipient = format!(
        "0x000000000000000000000000{}",
        args.recipient.trim_start_matches("0x")
    );

    // Derive forwarding address
    let forward_addr =
        forwarding_relayer::derive_forwarding_address(args.dest_domain, &dest_recipient)?;
    info!("Forwarding address: {}", forward_addr);

    // === Step 1: Verify services ===
    info!("\n--- Step 1: Verifying services ---");
    verify_anvil_running(&args.anvil_rpc).await?;
    info!("Anvil is running");

    verify_backend_running(&backend_url).await?;
    info!("Backend is running");

    // === Step 2: Query initial balance ===
    info!("\n--- Step 2: Querying initial balance ---");
    let initial_balance =
        query_erc20_balance(&args.anvil_rpc, &warp_token, &args.recipient).await?;
    info!("Initial wTIA balance: {}", initial_balance);

    // === Step 3: Create forwarding request ===
    info!("\n--- Step 3: Creating forwarding request ---");

    let http_client = reqwest::Client::new();
    let create_req = CreateForwardingRequest {
        forward_addr: forward_addr.clone(),
        dest_domain: args.dest_domain,
        dest_recipient: dest_recipient.clone(),
    };

    let resp = http_client
        .post(format!("{}/forwarding-requests", backend_url))
        .json(&create_req)
        .send()
        .await
        .context("Failed to create forwarding request")?;

    if !resp.status().is_success() {
        anyhow::bail!("Failed to create forwarding request: {}", resp.status());
    }

    let created: ForwardingRequest = resp.json().await?;
    info!(
        "Created forwarding request: {} for {}",
        created.id, forward_addr
    );

    // === Step 4: Fund forwarding address ===
    info!("\n--- Step 4: Funding forwarding address ---");
    // Fund the forwarding address — this triggers the relayer to detect a balance change
    info!(
        "Funding forwarding address {} with {}utia",
        forward_addr, args.fund_amount
    );
    fund_celestia_account(&forward_addr, args.fund_amount)?;

    // === Step 5: Wait for Hyperlane relay ===
    info!("\n--- Step 5: Waiting for forwarding + Hyperlane relay ---");
    info!(
        "Polling for balance change (timeout: {}s)...",
        args.timeout_secs
    );

    let poll_interval = Duration::from_secs(5);
    let timeout = Duration::from_secs(args.timeout_secs);
    let start = std::time::Instant::now();
    let mut final_balance = initial_balance;

    while start.elapsed() < timeout {
        tokio::time::sleep(poll_interval).await;

        match query_erc20_balance(&args.anvil_rpc, &warp_token, &args.recipient).await {
            Ok(balance) => {
                final_balance = balance;
                let elapsed = start.elapsed().as_secs();
                if balance > initial_balance {
                    info!(
                        "Balance changed after {}s! {} -> {}",
                        elapsed, initial_balance, balance
                    );
                    break;
                }
                info!(
                    "  Polling... ({}s/{}s) balance={}",
                    elapsed, args.timeout_secs, balance
                );
            }
            Err(e) => warn!("Failed to query balance: {:#}", e),
        }
    }

    // === Step 6: Verify final balance ===
    info!("\n--- Step 6: Results ---");
    info!("Initial balance: {}", initial_balance);
    info!("Final balance:   {}", final_balance);

    if final_balance > initial_balance {
        let forwarded = final_balance - initial_balance;
        info!(
            "\nSUCCESS! {} utia forwarded from Celestia to Anvil as wTIA",
            forwarded
        );
        Ok(())
    } else {
        anyhow::bail!(
            "FAILED: Balance did not increase (expected > {}, got {}).\n\
             Troubleshooting:\n\
             - Check Hyperlane relayer: docker logs relayer\n\
             - Check Celestia validator: docker logs celestia-validator",
            initial_balance,
            final_balance
        )
    }
}

/// Detect warp token address from Hyperlane deployment files
fn detect_warp_token() -> Result<String> {
    let content = std::fs::read_to_string(WARP_ROUTE_CONFIG_PATH)
        .context("Warp route config not found. Has Hyperlane deployment completed?")?;

    for line in content.lines() {
        if line.contains("addressOrDenom:") {
            if let Some(addr) = line
                .split(':')
                .nth(1)
                .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                .filter(|s| !s.is_empty())
            {
                return Ok(addr);
            }
        }
    }

    anyhow::bail!("addressOrDenom not found in {}", WARP_ROUTE_CONFIG_PATH)
}

/// Verify that Anvil is running and responsive
async fn verify_anvil_running(rpc_url: &str) -> Result<()> {
    let provider =
        Provider::<Http>::try_from(rpc_url).context("Failed to create Anvil provider")?;

    provider
        .get_block_number()
        .await
        .context("Failed to connect to Anvil. Is it running?")?;

    Ok(())
}

/// Verify that the backend is running and responsive
async fn verify_backend_running(backend_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    client
        .get(format!("{}/forwarding-requests", backend_url))
        .send()
        .await
        .context("Failed to connect to backend")?;

    Ok(())
}

/// Query ERC20 token balance on Anvil
async fn query_erc20_balance(rpc_url: &str, token_addr: &str, account: &str) -> Result<U256> {
    let provider = Provider::<Http>::try_from(rpc_url)?;

    let token: H160 = token_addr.parse().context("Invalid token address")?;
    let account: H160 = account.parse().context("Invalid account address")?;

    // ERC20 balanceOf(address) selector: 0x70a08231
    let mut data = vec![0x70, 0xa0, 0x82, 0x31];
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(account.as_bytes());

    let tx: TypedTransaction = TransactionRequest::new().to(token).data(data).into();
    let result = provider.call(&tx, None).await?;
    Ok(U256::from_big_endian(&result))
}

/// Fund a Celestia account by sending utia from the validator's default account
fn fund_celestia_account(address: &str, amount: u64) -> Result<()> {
    let output = std::process::Command::new("docker")
        .args([
            "exec",
            "celestia-validator",
            "celestia-appd",
            "tx",
            "bank",
            "send",
            "default",
            address,
            &format!("{}utia", amount),
            "--fees",
            "800utia",
            "--yes",
            "--chain-id",
            "celestia-zkevm-testnet",
            "--node",
            "http://localhost:26657",
        ])
        .output()
        .context("Failed to run docker exec. Is Docker running?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("Fund tx may have failed: {}", stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        info!("Fund tx: {}", stdout.trim());
    }

    Ok(())
}
