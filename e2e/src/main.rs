use anyhow::{Context, Result};
use clap::Parser;
use ethers::prelude::*;
use ethers::types::transaction::eip2718::TypedTransaction;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

use forwarding_relayer::{Backend, CreateForwardingRequest, ForwardingRequest};

const WARP_ROUTE_CONFIG_PATH: &str =
    "testnet/hyperlane/registry/deployments/warp_routes/TIA/celestiadev-rethlocal-config.yaml";

/// Distinct Anvil recipients (well-known dev accounts), one per scenario, so each
/// scenario watches an independent wTIA balance and they don't interfere.
const RECIPIENT_A: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const RECIPIENT_B: &str = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
const RECIPIENT_C: &str = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";
const RECIPIENT_D: &str = "0x90F79bf6EB2c4f870365E785982E1f101E93b906";
const RECIPIENT_E: &str = "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65";

#[derive(Debug, serde::Deserialize)]
struct WarpRouteConfig {
    tokens: Vec<WarpRouteToken>,
}

#[derive(Debug, serde::Deserialize)]
struct WarpRouteToken {
    #[serde(rename = "addressOrDenom")]
    address_or_denom: String,
    #[serde(rename = "chainName")]
    chain_name: String,
}

#[derive(Parser, Debug)]
#[command(name = "e2e")]
#[command(about = "End-to-end test for the forwarding relayer with Anvil")]
struct Args {
    /// Anvil RPC URL
    #[arg(long, default_value = "http://localhost:8545")]
    anvil_rpc: String,

    /// Warp token address on Anvil (auto-detected from deployment files if not provided)
    #[arg(long, env = "WARP_TOKEN")]
    warp_token: Option<String>,

    /// Backend port
    #[arg(long, default_value = "8080")]
    backend_port: u16,

    /// Amount to send to a forwarding address per deposit (in utia)
    #[arg(long, default_value = "1000000")]
    fund_amount: u64,

    /// How long to wait for a single deposit to be forwarded + Hyperlane-relayed (seconds)
    #[arg(long, default_value = "180")]
    relay_timeout_secs: u64,

    /// Must match the relayer's MAX_REQUEST_AGE_SECONDS: how long a never-funded
    /// address survives before it is retired from the live list.
    #[arg(long, default_value = "300")]
    max_request_age_secs: u64,

    /// Must match the relayer's MAINTENANCE_INTERVAL: how often the relayer runs
    /// its retention/refresh sweep (used to size waits for retirement).
    #[arg(long, default_value = "15")]
    maintenance_interval_secs: u64,

    /// Name of the relayer container (stopped/started in the restart scenario).
    #[arg(long, default_value = "forwarding-relayer")]
    relayer_container: String,

    /// Destination Hyperlane domain
    #[arg(long, default_value = "1234")]
    dest_domain: u32,

    /// Token ID (hex-encoded, auto-detected from deployment files if not provided)
    #[arg(long, env = "TOKEN_ID")]
    token_id: Option<String>,
}

/// Shared context for all scenarios.
struct Ctx {
    http: reqwest::Client,
    backend_url: String,
    anvil_rpc: String,
    warp_token: String,
    token_id: String,
    dest_domain: u32,
    fund_amount: u64,
    relay_timeout: Duration,
    max_request_age: Duration,
    maintenance_interval: Duration,
    relayer_container: String,
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
    info!("Anvil RPC:   {}", args.anvil_rpc);
    info!("Backend URL: {}", backend_url);

    let warp_token = match args.warp_token.clone() {
        Some(t) => t,
        None => read_anvil_warp_token().context(
            "Failed to auto-detect warp token. Provide --warp-token or set WARP_TOKEN env var",
        )?,
    };
    info!("Warp token: {}", warp_token);

    let token_id = match args.token_id.clone() {
        Some(t) => t,
        None => read_celestia_warp_token().context(
            "Failed to auto-detect token ID. Provide --token-id or set TOKEN_ID env var",
        )?,
    };
    info!("Token ID: {}", token_id);

    // Verify prerequisites before running any scenario.
    verify_anvil_running(&args.anvil_rpc).await?;
    info!("Anvil is running");
    verify_backend_running(&backend_url).await?;
    info!("Backend is running");

    let ctx = Ctx {
        http: reqwest::Client::new(),
        backend_url,
        anvil_rpc: args.anvil_rpc,
        warp_token,
        token_id,
        dest_domain: args.dest_domain,
        fund_amount: args.fund_amount,
        relay_timeout: Duration::from_secs(args.relay_timeout_secs),
        max_request_age: Duration::from_secs(args.max_request_age_secs),
        maintenance_interval: Duration::from_secs(args.maintenance_interval_secs),
        relayer_container: args.relayer_container,
    };

    // Run every scenario sequentially, collecting results so one failure doesn't
    // hide the others.
    let mut results: Vec<(&str, Result<()>)> = Vec::new();
    macro_rules! run {
        ($name:expr, $fut:expr) => {{
            info!("\n========== Scenario {} ==========", $name);
            let outcome = $fut.await;
            match &outcome {
                Ok(()) => info!("Scenario PASSED: {}", $name),
                Err(e) => error!("Scenario FAILED: {}: {:#}", $name, e),
            }
            results.push(($name, outcome));
        }};
    }

    run!(
        "A: register + immediate deposit",
        scenario_immediate_deposit(&ctx)
    );
    run!(
        "B: register + late deposit (retire) + re-register",
        scenario_late_deposit_reregister(&ctx)
    );
    run!(
        "C: register + multiple deposits in window + auto-relay",
        scenario_multiple_deposits(&ctx)
    );
    run!(
        "D: restart catch-up (crash + deposit while down)",
        scenario_restart_catch_up(&ctx)
    );
    run!(
        "E: rapid back-to-back deposits (no lost triggers)",
        scenario_rapid_back_to_back(&ctx)
    );
    run!(
        "F: rate limiting (default cap + whitelist bypass)",
        scenario_rate_limit()
    );

    info!("\n========== E2E Summary ==========");
    let mut failed = 0;
    for (name, outcome) in &results {
        match outcome {
            Ok(()) => info!("  PASS  {name}"),
            Err(e) => {
                failed += 1;
                error!("  FAIL  {name}: {e:#}");
            }
        }
    }

    if failed == 0 {
        info!("\nSUCCESS! All {} scenarios passed.", results.len());
        Ok(())
    } else {
        anyhow::bail!(
            "{failed}/{} scenario(s) failed. Troubleshooting:\n\
             - Forwarding relayer: docker logs {}\n\
             - Hyperlane relayer:  docker logs relayer\n\
             - Celestia validator: docker logs celestia-validator",
            results.len(),
            ctx.relayer_container,
        )
    }
}

/// Scenario A: register an intent, deposit immediately, expect the deposit to be
/// forwarded and relayed to Anvil as wTIA.
async fn scenario_immediate_deposit(ctx: &Ctx) -> Result<()> {
    let (dest_recipient, forward_addr) = derive(ctx, RECIPIENT_A)?;
    let baseline = ctx.erc20_balance(RECIPIENT_A).await?;

    ctx.register(&forward_addr, &dest_recipient).await?;
    // Give the relayer a maintenance cycle to pick the address into its live set.
    tokio::time::sleep(ctx.maintenance_interval + Duration::from_secs(3)).await;

    info!("Funding {forward_addr} with {}utia", ctx.fund_amount);
    fund_celestia_account(&forward_addr, ctx.fund_amount)?;

    ctx.wait_for_increase(RECIPIENT_A, baseline, ctx.relay_timeout)
        .await
        .context("immediate deposit was not relayed")?;
    Ok(())
}

/// Scenario B: register but never deposit; after MAX_REQUEST_AGE_SECONDS the relayer
/// must retire (drop) the address. Then re-register, deposit, and expect a relay —
/// proving a retired address can be revived by registering a new intent.
async fn scenario_late_deposit_reregister(ctx: &Ctx) -> Result<()> {
    let (dest_recipient, forward_addr) = derive(ctx, RECIPIENT_B)?;
    let baseline = ctx.erc20_balance(RECIPIENT_B).await?;

    let created = ctx.register(&forward_addr, &dest_recipient).await?;
    anyhow::ensure!(created, "expected a fresh registration for {forward_addr}");
    anyhow::ensure!(
        ctx.is_registered(&forward_addr).await?,
        "address should be registered immediately after POST"
    );

    // Wait past the unfunded retention window plus a maintenance cycle, then confirm
    // the relayer retired the never-funded address from the backend registry.
    let retire_wait = ctx.max_request_age + ctx.maintenance_interval + Duration::from_secs(30);
    info!(
        "Waiting {}s for the never-funded address to be retired...",
        retire_wait.as_secs()
    );
    tokio::time::sleep(retire_wait).await;
    anyhow::ensure!(
        !ctx.is_registered(&forward_addr).await?,
        "expected {forward_addr} to be retired after {}s without a deposit",
        retire_wait.as_secs()
    );
    info!("Confirmed: unfunded address was retired");

    // Re-register the same intent: this must create a brand-new request.
    let recreated = ctx.register(&forward_addr, &dest_recipient).await?;
    anyhow::ensure!(
        recreated,
        "re-registration after retirement should create a fresh request"
    );
    tokio::time::sleep(ctx.maintenance_interval + Duration::from_secs(3)).await;

    info!(
        "Funding re-registered {forward_addr} with {}utia",
        ctx.fund_amount
    );
    fund_celestia_account(&forward_addr, ctx.fund_amount)?;
    ctx.wait_for_increase(RECIPIENT_B, baseline, ctx.relay_timeout)
        .await
        .context("deposit after re-registration was not relayed")?;
    Ok(())
}

/// Scenario C: register once, then deposit several times within the inactivity window.
/// Each deposit must auto-relay without re-registering, proving the address stays on
/// the live list across multiple deposits.
async fn scenario_multiple_deposits(ctx: &Ctx) -> Result<()> {
    const DEPOSITS: usize = 3;
    let (dest_recipient, forward_addr) = derive(ctx, RECIPIENT_C)?;

    ctx.register(&forward_addr, &dest_recipient).await?;
    tokio::time::sleep(ctx.maintenance_interval + Duration::from_secs(3)).await;

    for round in 1..=DEPOSITS {
        let baseline = ctx.erc20_balance(RECIPIENT_C).await?;
        info!(
            "Deposit {round}/{DEPOSITS}: funding {forward_addr} with {}utia",
            ctx.fund_amount
        );
        fund_celestia_account(&forward_addr, ctx.fund_amount)?;
        ctx.wait_for_increase(RECIPIENT_C, baseline, ctx.relay_timeout)
            .await
            .with_context(|| format!("deposit {round} was not relayed"))?;
        info!("Deposit {round}/{DEPOSITS} relayed");
    }

    // The address should still be live (not retired) after repeated use.
    anyhow::ensure!(
        ctx.is_registered(&forward_addr).await?,
        "address should remain live after multiple in-window deposits"
    );
    Ok(())
}

/// Scenario D: register, stop the relayer, deposit while it is down, then restart it.
/// On restart the relayer must catch up (replay from its persisted scan cursor and
/// probe the live list) and relay the deposit it never saw live — proving indexing
/// survives a crash.
async fn scenario_restart_catch_up(ctx: &Ctx) -> Result<()> {
    let (dest_recipient, forward_addr) = derive(ctx, RECIPIENT_D)?;
    let baseline = ctx.erc20_balance(RECIPIENT_D).await?;

    ctx.register(&forward_addr, &dest_recipient).await?;
    tokio::time::sleep(ctx.maintenance_interval + Duration::from_secs(3)).await;

    info!("Stopping relayer container '{}'", ctx.relayer_container);
    docker(&["stop", &ctx.relayer_container]).context("failed to stop relayer container")?;

    // Deposit while the relayer is down, then let the tx commit into a block.
    info!(
        "Funding {forward_addr} with {}utia while relayer is down",
        ctx.fund_amount
    );
    fund_celestia_account(&forward_addr, ctx.fund_amount)?;
    tokio::time::sleep(Duration::from_secs(15)).await;

    info!("Restarting relayer container '{}'", ctx.relayer_container);
    docker(&["start", &ctx.relayer_container]).context("failed to start relayer container")?;

    // Allow extra time on top of the relay timeout for catch-up after restart.
    let timeout = ctx.relay_timeout + ctx.maintenance_interval + Duration::from_secs(30);
    ctx.wait_for_increase(RECIPIENT_D, baseline, timeout)
        .await
        .context("deposit made while the relayer was down was not relayed after restart")?;
    Ok(())
}

/// Scenario E: fire several deposits in quick succession so later ones land while
/// an earlier forward is still in flight. Every deposit must still be forwarded —
/// none dropped by the in-flight dedup — so the recipient ends up with the full
/// total. Exercises the dirty-marker re-enqueue path.
async fn scenario_rapid_back_to_back(ctx: &Ctx) -> Result<()> {
    const DEPOSITS: u64 = 3;
    let (dest_recipient, forward_addr) = derive(ctx, RECIPIENT_E)?;
    let baseline = ctx.erc20_balance(RECIPIENT_E).await?;

    ctx.register(&forward_addr, &dest_recipient).await?;
    tokio::time::sleep(ctx.maintenance_interval + Duration::from_secs(3)).await;

    // Send deposits back-to-back. We wait only long enough between funding txs for
    // each to commit (so the sender's account sequence advances) — far less than a
    // full relay — so later deposits arrive mid-forward of earlier ones.
    for i in 1..=DEPOSITS {
        info!("Rapid deposit {i}/{DEPOSITS} -> {forward_addr}");
        fund_celestia_account(&forward_addr, ctx.fund_amount)?;
        tokio::time::sleep(Duration::from_secs(4)).await;
    }

    let target = baseline + U256::from(ctx.fund_amount * DEPOSITS);
    ctx.wait_for_at_least(
        RECIPIENT_E,
        target,
        ctx.relay_timeout + ctx.maintenance_interval,
    )
    .await
    .context("not all rapid back-to-back deposits were relayed")?;
    Ok(())
}

/// Scenario F: verify the backend's per-IP rate limiting. This is self-contained:
/// it starts two throwaway in-process backends (each with its own tiny rate-limit
/// config) so it does not interfere with the shared dockerized backend used by the
/// other scenarios.
///
/// - A non-whitelisted client gets the default budget (1/min): the first POST is
///   accepted, the second is rejected with `429` + a `Retry-After` header.
/// - A whitelisted client (here, loopback) bypasses the low default and can burst
///   well past it without being limited.
async fn scenario_rate_limit() -> Result<()> {
    // A request body whose forward_addr will not match derivation. The handler
    // rejects it with 400 — but only *after* the rate-limit middleware runs, so a
    // non-429 status proves the request was admitted by the limiter.
    let body = CreateForwardingRequest {
        forward_addr: "celestia1ratelimittest".to_string(),
        dest_domain: 1,
        dest_recipient: "0x00".to_string(),
        token_id: "0x00".to_string(),
    };
    let client = reqwest::Client::new();

    // --- Part 1: default cap of 1/min, no whitelist -> second POST is limited. ---
    let capped_port = 18080;
    start_rate_limited_backend(capped_port, 1, None).await?;
    let url = format!("http://127.0.0.1:{capped_port}/forwarding-requests");

    let first = client.post(&url).json(&body).send().await?;
    anyhow::ensure!(
        first.status() != reqwest::StatusCode::TOO_MANY_REQUESTS,
        "first request should be admitted, got {}",
        first.status()
    );

    let second = client.post(&url).json(&body).send().await?;
    anyhow::ensure!(
        second.status() == reqwest::StatusCode::TOO_MANY_REQUESTS,
        "second request within the minute should be rate limited, got {}",
        second.status()
    );
    anyhow::ensure!(
        second.headers().contains_key(reqwest::header::RETRY_AFTER),
        "429 response must carry a Retry-After header"
    );
    info!("Default cap honored: 2nd request returned 429 with Retry-After");

    // --- Part 2: loopback whitelisted at a high budget -> bursts are admitted. ---
    let wl_port = 18081;
    start_rate_limited_backend(wl_port, 1, Some(6000)).await?;
    let wl_url = format!("http://127.0.0.1:{wl_port}/forwarding-requests");

    const BURST: usize = 50;
    for i in 0..BURST {
        let resp = client.post(&wl_url).json(&body).send().await?;
        anyhow::ensure!(
            resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS,
            "whitelisted client should not be limited, but request {i} got 429"
        );
    }
    info!("Whitelist bypass honored: {BURST} rapid requests all admitted");

    Ok(())
}

/// Start a throwaway in-process backend with rate limiting enabled, listening on
/// `port`, and wait until it is reachable.
///
/// `default_per_minute` is the budget for non-whitelisted IPs. When
/// `whitelist_loopback` is `Some(n)`, `127.0.0.1`/`::1` are whitelisted at budget `n`.
async fn start_rate_limited_backend(
    port: u16,
    default_per_minute: u32,
    whitelist_loopback: Option<u32>,
) -> Result<()> {
    let apps = match whitelist_loopback {
        Some(per_minute) => serde_json::json!([{
            "name": "loopback",
            "hosts": ["127.0.0.1", "::1"],
            "per_minute": per_minute,
        }]),
        None => serde_json::json!([]),
    };
    let config = serde_json::json!({
        "default_per_minute": default_per_minute,
        "whitelist_default_per_minute": 6000,
        "apps": apps,
    });

    let config_path = std::env::temp_dir().join(format!("e2e_rate_limit_{port}.json"));
    std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)
        .context("failed to write rate-limit config")?;
    let db_path = std::env::temp_dir().join(format!("e2e_rate_limit_{port}.db"));

    let backend = Backend::with_rate_limiter(port, db_path, false, Some(config_path))
        .context("failed to construct rate-limited backend")?;
    tokio::spawn(async move {
        if let Err(e) = backend.serve().await {
            error!("rate-limit test backend on port {port} exited: {e:#}");
        }
    });

    // Wait for the listener to accept connections.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let health_url = format!("http://127.0.0.1:{port}/forwarding-requests");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if client.get(&health_url).send().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("rate-limit test backend on port {port} did not become ready");
}

impl Ctx {
    /// POST a forwarding request. Returns true if a new request was created (201),
    /// false if an existing one was returned (200).
    async fn register(&self, forward_addr: &str, dest_recipient: &str) -> Result<bool> {
        let create_req = CreateForwardingRequest {
            forward_addr: forward_addr.to_string(),
            dest_domain: self.dest_domain,
            dest_recipient: dest_recipient.to_string(),
            token_id: self.token_id.clone(),
        };
        let resp = self
            .http
            .post(format!("{}/forwarding-requests", self.backend_url))
            .json(&create_req)
            .send()
            .await
            .context("Failed to create forwarding request")?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("Failed to create forwarding request: {status}");
        }
        info!("Registered forwarding request for {forward_addr} (status {status})");
        Ok(status == reqwest::StatusCode::CREATED)
    }

    /// Whether the backend currently lists a request for `forward_addr`.
    async fn is_registered(&self, forward_addr: &str) -> Result<bool> {
        let resp = self
            .http
            .get(format!("{}/forwarding-requests", self.backend_url))
            .send()
            .await
            .context("Failed to list forwarding requests")?;
        let requests: Vec<ForwardingRequest> = resp.json().await?;
        Ok(requests.iter().any(|r| r.forward_addr == forward_addr))
    }

    async fn erc20_balance(&self, recipient: &str) -> Result<U256> {
        query_erc20_balance(&self.anvil_rpc, &self.warp_token, recipient).await
    }

    /// Poll the recipient's wTIA balance until it rises above `baseline` or the
    /// timeout elapses. Returns the new balance, or an error on timeout.
    async fn wait_for_increase(
        &self,
        recipient: &str,
        baseline: U256,
        timeout: Duration,
    ) -> Result<U256> {
        let poll_interval = Duration::from_secs(5);
        let start = Instant::now();
        while start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;
            match self.erc20_balance(recipient).await {
                Ok(balance) if balance > baseline => {
                    info!(
                        "Balance increased after {}s: {} -> {}",
                        start.elapsed().as_secs(),
                        baseline,
                        balance
                    );
                    return Ok(balance);
                }
                Ok(balance) => info!(
                    "  Polling... ({}s/{}s) balance={}",
                    start.elapsed().as_secs(),
                    timeout.as_secs(),
                    balance
                ),
                Err(e) => warn!("Failed to query balance: {e:#}"),
            }
        }
        anyhow::bail!(
            "balance for {recipient} did not increase above {baseline} within {}s",
            timeout.as_secs()
        )
    }

    /// Poll the recipient's wTIA balance until it reaches at least `target` or the
    /// timeout elapses. Used to assert that *every* deposit was forwarded.
    async fn wait_for_at_least(
        &self,
        recipient: &str,
        target: U256,
        timeout: Duration,
    ) -> Result<U256> {
        let poll_interval = Duration::from_secs(5);
        let start = Instant::now();
        while start.elapsed() < timeout {
            tokio::time::sleep(poll_interval).await;
            match self.erc20_balance(recipient).await {
                Ok(balance) if balance >= target => {
                    info!(
                        "Balance reached target after {}s: {} (>= {})",
                        start.elapsed().as_secs(),
                        balance,
                        target
                    );
                    return Ok(balance);
                }
                Ok(balance) => info!(
                    "  Polling... ({}s/{}s) balance={} target={}",
                    start.elapsed().as_secs(),
                    timeout.as_secs(),
                    balance,
                    target
                ),
                Err(e) => warn!("Failed to query balance: {e:#}"),
            }
        }
        anyhow::bail!(
            "balance for {recipient} did not reach {target} within {}s",
            timeout.as_secs()
        )
    }
}

/// Derive the 32-byte-padded dest_recipient and the forwarding address for an
/// Anvil recipient.
fn derive(ctx: &Ctx, recipient: &str) -> Result<(String, String)> {
    let dest_recipient = format!(
        "0x000000000000000000000000{}",
        recipient.trim_start_matches("0x")
    );
    let forward_addr = forwarding_relayer::derive_forwarding_address(
        ctx.dest_domain,
        &dest_recipient,
        &ctx.token_id,
    )?;
    Ok((dest_recipient, forward_addr))
}

/// Returns the celestia collateral token ID in Hyperlane HexAddress string format.
fn read_celestia_warp_token() -> Result<String> {
    let config = read_warp_route_config()?;
    config
        .tokens
        .into_iter()
        .find(|token| token.chain_name == "celestiadev")
        .map(|token| token.address_or_denom)
        .context("Celestia collateral token ID not found in warp route config")
}

/// Returns the anvil synthetic token address in Ethereum address string format.
fn read_anvil_warp_token() -> Result<String> {
    let config = read_warp_route_config()?;
    config
        .tokens
        .into_iter()
        .find(|token| token.chain_name == "rethlocal")
        .map(|token| token.address_or_denom)
        .context("rethlocal synthetic token address not found in warp route config")
}

fn read_warp_route_config() -> Result<WarpRouteConfig> {
    let content = std::fs::read_to_string(WARP_ROUTE_CONFIG_PATH)
        .context("Warp route config not found. Has Hyperlane deployment completed?")?;
    serde_yaml::from_str(&content).context("Failed to parse warp route config")
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
            "celestiadev",
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

/// Run a `docker` CLI command, failing if it returns a non-zero exit code.
fn docker(args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("docker")
        .args(args)
        .output()
        .context("Failed to run docker. Is Docker running?")?;
    if !output.status.success() {
        anyhow::bail!(
            "docker {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
