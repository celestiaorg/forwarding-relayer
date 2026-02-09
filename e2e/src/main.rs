use anyhow::{Context, Result};
use clap::Parser;
use ethers::prelude::*;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(name = "e2e")]
#[command(about = "End-to-end test for forwarding relayer with Anvil", long_about = None)]
struct Args {
    /// Anvil RPC URL
    #[arg(long, default_value = "http://localhost:8545")]
    anvil_rpc: String,

    /// Warp token address on Anvil (synthetic TIA token)
    #[arg(long)]
    warp_token: Option<String>,

    /// Recipient address on Anvil
    #[arg(long, default_value = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")]
    recipient: String,

    /// Backend URL
    #[arg(long, default_value = "http://localhost:8080")]
    backend_url: String,

    /// Skip initial balance check
    #[arg(long)]
    skip_initial_check: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    info!("Starting E2E test");
    info!("Anvil RPC: {}", args.anvil_rpc);
    info!("Backend URL: {}", args.backend_url);
    info!("Recipient: {}", args.recipient);

    // Step 1: Verify Docker services are running
    info!("Step 1: Verifying services are running...");
    verify_anvil_running(&args.anvil_rpc).await?;
    verify_backend_running(&args.backend_url).await?;
    info!("✓ All services are running");

    // Step 2: Query initial balance on Anvil
    if !args.skip_initial_check {
        if let Some(ref token_addr) = args.warp_token {
            info!("Step 2: Querying initial balance on Anvil...");
            let initial_balance = query_erc20_balance(&args.anvil_rpc, token_addr, &args.recipient)
                .await
                .context("Failed to query initial balance")?;
            info!("Initial balance: {} utia", initial_balance);

            if initial_balance > 0 {
                warn!("WARNING: Initial balance is not 0. This may indicate a previous test run.");
                warn!("Consider resetting the Anvil state for a clean test.");
            }
        } else {
            warn!("Skipping initial balance check: warp token address not provided");
        }
    }

    // Step 3-7: These would be implemented in a full E2E test
    info!("\nManual E2E testing steps:");
    info!("3. Create a forwarding request via the backend API");
    info!("4. Fund the forwarding address on Celestia");
    info!("5. Start the forwarding relayer");
    info!("6. Wait for Hyperlane relayer to process the message (10-30s)");
    info!("7. Query final balance on Anvil (should be > 0)");

    if let Some(ref token_addr) = args.warp_token {
        info!("\nTo query balance after forwarding:");
        info!("  cast call {} 'balanceOf(address)(uint256)' {} --rpc-url {}",
            token_addr, args.recipient, args.anvil_rpc);
    }

    info!("\nE2E test setup verified successfully!");
    Ok(())
}

/// Verify that Anvil is running and responsive
async fn verify_anvil_running(rpc_url: &str) -> Result<()> {
    let provider = Provider::<Http>::try_from(rpc_url)
        .context("Failed to create Anvil provider")?;

    provider
        .get_block_number()
        .await
        .context("Failed to connect to Anvil")?;

    Ok(())
}

/// Verify that the backend is running and responsive
async fn verify_backend_running(backend_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let response = client
        .get(format!("{}/forwarding-requests", backend_url))
        .send()
        .await
        .context("Failed to connect to backend")?;

    if !response.status().is_success() {
        anyhow::bail!("Backend returned non-success status: {}", response.status());
    }

    Ok(())
}

/// Query ERC20 token balance
async fn query_erc20_balance(rpc_url: &str, token_addr: &str, account: &str) -> Result<U256> {
    let provider = Provider::<Http>::try_from(rpc_url)?;

    let token: H160 = token_addr
        .parse()
        .context("Invalid token address")?;
    let account: H160 = account
        .parse()
        .context("Invalid account address")?;

    // ERC20 balanceOf(address) function signature: 0x70a08231
    let mut data = vec![0x70, 0xa0, 0x82, 0x31];
    // Pad account address to 32 bytes
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(account.as_bytes());

    let tx = ethers::types::transaction::eip2718::TypedTransaction::default()
        .to(token)
        .data(data);

    let result = provider.call(&tx, None).await?;
    let balance = U256::from_big_endian(&result);

    Ok(balance)
}
