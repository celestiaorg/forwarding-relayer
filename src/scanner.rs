//! Event-driven deposit detection.
//!
//! Instead of polling every known forwarding address for its balance each round
//! (O(N) gRPC calls per interval), the scanner reads each committed block once and
//! extracts the bank deposit events (`coin_received` / `transfer`) it contains. Work
//! is then O(deposits per block) plus an O(1) membership check per transfer, which
//! scales to hundreds of thousands of watched addresses.
//!
//! CometBFT has instant finality (no reorgs on committed blocks), so scanning the
//! committed `block_results` for a height is safe and never needs to be undone.
//!
//! The one subtlety is *read-after-write*, not finality: a node can deliver the
//! `NewBlock` notification for height H (from consensus) microseconds before its own
//! RPC layer has H's `FinalizeBlock` tx-results durably queryable, so
//! `block_results(H)` can briefly return HTTP 200 with no tx events for the
//! just-committed tip. To avoid reading into that window the scanner stays a small
//! `confirmation_depth` of blocks behind the tip (default 2), by which point the node
//! has long since indexed the block. This is purely an indexing-lag margin, not reorg
//! protection.
//!
//! # Block_Results Cursor Recovery
//!
//! The cursor normally advances only on a successful `block_results` fetch, so a
//! height that no endpoint can serve would pruned it forever. Cursor recovery
//! skips past a stuck height only on *evidence* that waiting
//! cannot fix it: immediately when every configured endpoint reports the height
//! pruned (below its `earliest_block_height`), and after a timeout when an
//! endpoint retains the height but persistently fails to serve it.
//! In the event where a height temporarily unavailable, due to the node being down, resyncing, or unreachable -
//! is retried indefinitely, and event-driven scanning resumes from the exact cursor
//! once the node recovers, replaying every retained block with no gap. Deposits in
//! any range that *is* skipped are recovered by the relayer's balance-poll sweep,
//! which detects funds by balance and does not depend on any particular block.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use metrics::{counter, gauge};
use tendermint::abci;
use tendermint::block::Height;
use tendermint_rpc::endpoint::block_results;
use tendermint_rpc::event::EventData;
use tendermint_rpc::query::{EventType, Query};
use tendermint_rpc::{Client, HttpClient, SubscriptionClient, WebSocketClient};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, warn};

use crate::relayer::RetryStore;
use crate::{parse_endpoint_specs, AuthToken, Balance, EndpointSpec, ForwardingRequest};

/// Delay before re-establishing the block subscription after it ends or errors.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Shared map of the addresses currently being watched (the live list).
type LiveSet = Arc<Mutex<HashMap<String, ForwardingRequest>>>;

/// A detected inbound deposit to an address within a single block. `coins` is parsed
/// best-effort from the event's `amount` attribute and is informational only — the
/// forward path re-queries the authoritative balance before submitting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deposit {
    pub recipient: String,
    pub coins: Vec<Balance>,
}

/// Extract all deposits from a block's results, scanning transaction events as well
/// as block-level (finalize / begin / end) events so module-minted deposits are
/// caught alongside ordinary transfers. Deduplicated by recipient: a recipient that
/// appears in multiple events (e.g. both `coin_received` and `transfer`, or several
/// transfers) yields a single `Deposit` — the forward path re-reads the real balance.
pub fn extract_deposits(results: &block_results::Response) -> Vec<Deposit> {
    let tx_events = results
        .txs_results
        .iter()
        .flatten()
        .flat_map(|tx| tx.events.iter());
    let finalize_events = results.finalize_block_events.iter();
    let begin_events = results.begin_block_events.iter().flatten();
    let end_events = results.end_block_events.iter().flatten();

    deposits_from_events(
        tx_events
            .chain(finalize_events)
            .chain(begin_events)
            .chain(end_events),
    )
}

/// Core pure parser: scan a stream of ABCI events for bank deposit events and return
/// one deduplicated `Deposit` per recipient, preserving first-seen order.
fn deposits_from_events<'a>(events: impl Iterator<Item = &'a abci::Event>) -> Vec<Deposit> {
    let mut deposits = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for event in events {
        // The recipient attribute is named differently across the two equivalent
        // bank events; `coin_spent` (the debit side) is intentionally ignored.
        let recipient_key = match event.kind.as_str() {
            "coin_received" => "receiver",
            "transfer" => "recipient",
            _ => continue,
        };

        let mut recipient = None;
        let mut amount = None;
        for attr in &event.attributes {
            match attr.key_str() {
                Ok(key) if key == recipient_key => recipient = attr.value_str().ok(),
                Ok("amount") => amount = attr.value_str().ok(),
                _ => {}
            }
        }

        if let Some(recipient) = recipient {
            if seen.insert(recipient.to_string()) {
                deposits.push(Deposit {
                    recipient: recipient.to_string(),
                    coins: amount.map(parse_coins).unwrap_or_default(),
                });
            }
        }
    }

    deposits
}

/// Parse a Cosmos coins string such as `"1000utia,500uatom"` into balances.
/// Lenient: tokens that don't start with digits or carry no denom are skipped,
/// since the value is informational only.
fn parse_coins(amount: &str) -> Vec<Balance> {
    amount
        .split(',')
        .filter_map(|token| {
            let token = token.trim();
            let split = token.find(|c: char| !c.is_ascii_digit())?;
            if split == 0 {
                return None; // no leading amount
            }
            Some(Balance {
                amount: token[..split].to_string(),
                denom: token[split..].to_string(),
            })
        })
        .collect()
}

/// Derive the WebSocket URL from an HTTP(S) CometBFT RPC URL
/// (e.g. `http://host:26657` -> `ws://host:26657/websocket`).
fn derive_ws_url(rpc_url: &str) -> String {
    let base = rpc_url.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{ws}/websocket")
}

/// Strip HTTP Basic userinfo (`user[:pass]@`) from a URL so the token spliced into
/// `ws_url` by [`with_auth_token`] never reaches logs on a WS connect error. The
/// credential is replaced with `***`; credential-free URLs are returned unchanged.
fn redact_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    // The authority ends at the first '/', '?', or '#'.
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    match authority.rsplit_once('@') {
        Some((_creds, host)) => format!("{scheme}://***@{host}{tail}"),
        None => url.to_string(),
    }
}

/// Build the connection URL for an endpoint, splicing `token` in as HTTP Basic
/// userinfo when set. The token is placed in the **password** field with an empty
/// username (`:token@host`), so the wire credential is `Authorization: Basic
/// base64(":token")` on both the HTTP requests and the WebSocket handshake.
///
/// The token is restricted to URL-safe characters, so it is inserted verbatim.
/// Returns the URL unchanged when no token is given. The token comes from the
/// `|token` part of the `CELESTIA_RPC` entry (never the URL itself), and is spliced
/// in here only for the connection, so it never appears in the stored/logged URL.
fn with_auth_token(url: &str, token: Option<&str>) -> Result<String> {
    let Some(token) = token else {
        return Ok(url.to_string());
    };
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("Invalid CometBFT RPC URL (expected scheme://host): {url}"))?;
    Ok(format!("{scheme}://:{token}@{rest}"))
}

async fn current_height(http: &HttpClient) -> Result<u64> {
    let status = http.status().await.context("Failed to query node status")?;
    Ok(status.sync_info.latest_block_height.value())
}

/// One CometBFT RPC endpoint: an HTTP client for `block_results`/`status` and the
/// WebSocket URL derived from it for the `NewBlock` trigger.
struct RpcEndpoint {
    url: String,
    http: HttpClient,
    ws_url: String,
}

/// An ordered set of interchangeable CometBFT RPC endpoints. The first is the
/// preferred primary and the rest are fallbacks. The scanner runs one session
/// against one endpoint at a time and rotates to the next on failure, so a single
/// unhealthy node degrades to its neighbor instead of pruneding deposit detection.
struct RpcPool {
    endpoints: Vec<RpcEndpoint>,
}

impl RpcPool {
    /// Parse a comma-separated list of `url|token` (or bare `url`) endpoints into a
    /// pool, building one HTTP client per endpoint. A single URL yields a one-endpoint
    /// pool, i.e. the original no-failover behavior.
    ///
    /// Each endpoint's optional token is spliced into *its* URL as HTTP Basic
    /// userinfo, so tendermint-rpc authenticates that endpoint's HTTP calls and WS
    /// subscription. The token lives in the `|token` part, so the stored `url` used
    /// for logging is always credential-free.
    fn new(spec: &str) -> Result<Self> {
        let endpoints = parse_endpoint_specs(spec)?
            .into_iter()
            .map(|EndpointSpec { url, token }| {
                let conn_url = with_auth_token(&url, token.as_ref().map(AuthToken::as_str))?;
                let http = HttpClient::new(conn_url.as_str())
                    .with_context(|| format!("Invalid CometBFT RPC URL: {url}"))?;
                Ok(RpcEndpoint {
                    url,
                    http,
                    ws_url: derive_ws_url(&conn_url),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        anyhow::ensure!(
            !endpoints.is_empty(),
            "CELESTIA_RPC contained no usable URLs"
        );
        Ok(Self { endpoints })
    }

    fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Comma-separated list of endpoint URLs, for logging.
    fn url_list(&self) -> String {
        self.endpoints
            .iter()
            .map(|e| e.url.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Query the chain tip, trying each endpoint in order until one answers. Used for
    /// the one-time cursor bootstrap so startup survives the primary being down.
    async fn current_height(&self) -> Result<u64> {
        let mut last_err = None;
        for ep in &self.endpoints {
            match current_height(&ep.http).await {
                Ok(height) => return Ok(height),
                Err(e) => {
                    warn!("Status query failed on {}: {e:#}", ep.url);
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no RPC endpoints")))
            .context("All RPC endpoints failed to return chain status")
    }

    /// Ask every endpoint what it knows about a height the scanner is stuck on: its
    /// retention horizon from `/status`, plus a direct `block_results` probe when the
    /// endpoint claims it should be able to serve the height. An endpoint whose
    /// status query fails contributes nothing — absence of testimony, not testimony —
    /// so a temporary outage can never masquerade as evidence for skipping.
    async fn gather_height_evidence(
        &self,
        stuck: u64,
        confirmation_depth: u64,
    ) -> Vec<HeightEvidence> {
        let Ok(block_height) = Height::try_from(stuck) else {
            return Vec::new();
        };
        let mut evidence = Vec::new();
        for ep in &self.endpoints {
            let status = match ep.http.status().await {
                Ok(status) => status,
                Err(e) => {
                    warn!(
                        "Status query failed on {} while assessing stuck height {stuck}: {e:#}",
                        ep.url
                    );
                    continue;
                }
            };
            let earliest = status.sync_info.earliest_block_height.value();
            let latest = status.sync_info.latest_block_height.value();
            // Probe only a node that retains the height AND whose tip is at least
            // `confirmation_depth` past it: a node still syncing toward the height
            // (or inside the tip read-after-write indexing window) would fail the
            // fetch for reasons that waiting fixes, and must not count as broken.
            let probe = if earliest <= stuck && stuck.saturating_add(confirmation_depth) <= latest {
                Some(ep.http.block_results(block_height).await.is_ok())
            } else {
                None
            };
            evidence.push(HeightEvidence { earliest, probe });
        }
        evidence
    }
}

/// One endpoint's live testimony about a height the scanner is stuck on.
struct HeightEvidence {
    /// Lowest height the endpoint still retains (`sync_info.earliest_block_height`).
    earliest: u64,
    /// Direct `block_results(stuck)` probe: `Some(true)` served, `Some(false)`
    /// errored, `None` not attempted (the endpoint has pruned the height, or its tip
    /// is not yet far enough past it for a failure to be meaningful).
    probe: Option<bool>,
}

/// Verdict on a stuck height, derived purely from gathered [`HeightEvidence`].
#[derive(Debug, PartialEq, Eq)]
enum PrunedVerdict {
    /// Every endpoint that answered has pruned the height; no amount of retrying can
    /// fetch it. Resume from `new_cursor + 1`, the lowest height any of them still
    /// retains. `unanimous` is true when every *configured* endpoint testified — a
    /// non-unanimous verdict leaves open that an unreachable endpoint still holds
    /// the blocks, so the caller waits out the timeout before acting on it.
    Pruned { new_cursor: u64, unanimous: bool },
    /// At least one endpoint retains the height yet fails to serve it, and none can
    /// serve it: a height-specific serving failure (corrupt or unindexed block),
    /// not a connectivity problem. Skipped only after the timeout.
    Unservable,
    /// The height was served by some endpoint, a retaining endpoint may still be
    /// syncing toward it, or no endpoint answered at all: waiting may fix it, so
    /// never skip.
    Wait,
}

/// Judge a stuck height from endpoint testimony. Pure, so the skip policy — the
/// safety-critical part of pruned recovery; is unit-testable without a network.
fn assess_evidence(
    evidence: &[HeightEvidence],
    total_endpoints: usize,
    stuck: u64,
) -> PrunedVerdict {
    if evidence.iter().any(|e| e.probe == Some(true)) {
        return PrunedVerdict::Wait; // fetchable right now; the next session consumes it
    }
    let Some(min_earliest) = evidence.iter().map(|e| e.earliest).min() else {
        return PrunedVerdict::Wait; // nobody answered: an outage, not height evidence
    };
    if min_earliest > stuck {
        return PrunedVerdict::Pruned {
            new_cursor: min_earliest - 1,
            unanimous: evidence.len() == total_endpoints,
        };
    }
    if evidence.iter().any(|e| e.probe == Some(false)) {
        return PrunedVerdict::Unservable;
    }
    PrunedVerdict::Wait // a holder exists but is still syncing toward the height
}

/// The height the scanner is currently stuck on and when it first got stuck there.
/// The clock resets whenever the cursor moves (a new stuck height) or a session
/// ends cleanly.
struct Pruned {
    height: u64,
    since: Instant,
}

/// Run the event-driven block scanner forever.
///
/// Maintains a strictly-monotonic height cursor (persisted after every block) and
/// uses a WebSocket `NewBlock` subscription only as a trigger; the authoritative
/// per-height fetch is `block_results` over HTTP, so a dropped/closed subscription
/// resumes from the persisted cursor with no missed blocks. For every detected
/// deposit whose recipient is in the live set, the recipient is sent to `deposits_tx`.
///
/// `rpc_url` may be a comma-separated list of equivalent CometBFT RPC endpoints; the
/// first is the primary and the rest are fallbacks. Each scan session runs against a
/// single endpoint and a session failure rotates to the next, so one unhealthy node
/// fails over to its neighbor. Because the cursor only advances on success (or on
/// proven-unrecoverable pruned recovery — see the module docs), failing over
/// mid-stream never skips a block — the next endpoint resumes where this one left off.
///
/// `timeout` bounds how long the scanner stays stuck on a single height whose
/// unavailability is not yet *proven* permanent; see [`recover_pruned`].
pub(crate) async fn run_block_scanner(
    rpc_url: String,
    start_height: Option<u64>,
    confirmation_depth: u64,
    timeout: Duration,
    live: LiveSet,
    store: Arc<Mutex<RetryStore>>,
    deposits_tx: Sender<String>,
) -> Result<()> {
    let pool = RpcPool::new(&rpc_url)?;
    info!(
        "Block scanner using {} RPC endpoint(s): {}",
        pool.len(),
        pool.url_list()
    );

    // Establish the starting cursor: persisted height, else configured start
    // (scanned inclusively), else the current chain tip. Bind the load result so
    // the mutex guard is dropped before any `.await`.
    let persisted = store.lock().unwrap().load_height();
    let mut cursor = match persisted {
        Ok(Some(height)) => height,
        Ok(None) => match start_height {
            Some(height) => height.saturating_sub(1),
            None => {
                let tip = pool.current_height().await?;
                info!("No persisted scan cursor; starting from chain tip {tip}");
                tip
            }
        },
        Err(e) => {
            warn!("Failed to load scan cursor, starting from chain tip: {e:#}");
            pool.current_height().await?
        }
    };
    if let Err(e) = store.lock().unwrap().store_height(cursor) {
        warn!("Failed to persist initial scan cursor: {e:#}");
    }
    info!(
        "Block scanner starting at cursor height {cursor} (confirmation depth {confirmation_depth})"
    );

    // Round-robin failover index: each session runs against one endpoint, and a
    // session failure advances to the next so an unhealthy node is skipped on the
    // next attempt. A clean subscription end keeps the same (working) endpoint.
    let mut current = 0;
    // Stuck-height tracker for pruned recovery, carried across sessions so repeated
    // failures on the same height accumulate toward the pruned timeout.
    let mut pruned: Option<Pruned> = None;
    loop {
        let endpoint = &pool.endpoints[current];
        match scan_session(
            &endpoint.http,
            &endpoint.ws_url,
            &mut cursor,
            confirmation_depth,
            &live,
            &store,
            &deposits_tx,
        )
        .await
        {
            Ok(()) => {
                pruned = None;
                warn!(
                    "Block subscription ended on {}; reconnecting in {}s",
                    endpoint.url,
                    RECONNECT_DELAY.as_secs()
                );
            }
            Err(e) => {
                error!(
                    "Block scan session failed on {}: {e:#}; reconnecting in {}s",
                    endpoint.url,
                    RECONNECT_DELAY.as_secs()
                );
                if pool.len() > 1 {
                    current = (current + 1) % pool.len();
                    counter!("relayer_rpc_failover_total").increment(1);
                    warn!(
                        "Failing over to RPC endpoint {}",
                        pool.endpoints[current].url
                    );
                }
                recover_pruned(
                    &pool,
                    &mut cursor,
                    &mut pruned,
                    timeout,
                    confirmation_depth,
                    &store,
                )
                .await;
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Assess a failed scan session for a stuck cursor and skip past the stuck height
/// only when live evidence proves waiting cannot fix it.
///
/// Skips happen in exactly two shapes, both leaving recovery of the skipped
/// deposits to the balance-poll sweep (which detects funds by balance, independent
/// of blocks):
///
/// - **Pruned**: every reachable endpoint reports the height below its retention
///   horizon. The cursor jumps to just before the lowest height any endpoint still
///   retains — immediately when all configured endpoints testified, or after
///   `timeout` when some were unreachable (an unreachable node may yet return
///   holding the blocks, in which case event-driven scanning replays them in full).
/// - **Unservable**: some endpoint retains the height yet persistently fails to
///   serve it (e.g. a corrupt or unindexed block). Only that single height is
///   skipped, and only after `timeout`.
///
/// Everything else — nodes syncing toward the height, RPC outages, WebSocket
/// failures — is a [`PrunedVerdict::Wait`]: the cursor stays put and event-driven
/// scanning resumes from it, gap-free, as soon as a node can serve the height.
async fn recover_pruned(
    pool: &RpcPool,
    cursor: &mut u64,
    pruned: &mut Option<Pruned>,
    timeout: Duration,
    confirmation_depth: u64,
    store: &Arc<Mutex<RetryStore>>,
) {
    let stuck = *cursor + 1;
    let since = match pruned {
        Some(s) if s.height == stuck => s.since,
        _ => {
            let now = Instant::now();
            *pruned = Some(Pruned {
                height: stuck,
                since: now,
            });
            now
        }
    };
    let elapsed = since.elapsed();

    let evidence = pool.gather_height_evidence(stuck, confirmation_depth).await;
    match assess_evidence(&evidence, pool.len(), stuck) {
        PrunedVerdict::Pruned {
            new_cursor,
            unanimous,
        } => {
            if unanimous || elapsed >= timeout {
                let skipped = new_cursor - *cursor;
                warn!(
                    "Heights {stuck}..={new_cursor} are pruned on every reachable RPC endpoint; \
                     skipping {skipped} block(s) to resume scanning at {}. Deposits in the gap \
                     will be recovered by the balance-poll sweep",
                    new_cursor + 1
                );
                counter!("relayer_scan_blocks_skipped_total", "reason" => "pruned")
                    .increment(skipped);
                advance_cursor(cursor, new_cursor, store);
                *pruned = None;
            } else {
                warn!(
                    "Height {stuck} is pruned on every reachable endpoint, but {} of {} \
                     endpoint(s) did not answer; waiting up to {}s more for them before skipping",
                    pool.len() - evidence.len(),
                    pool.len(),
                    timeout.saturating_sub(elapsed).as_secs()
                );
            }
        }
        PrunedVerdict::Unservable => {
            if elapsed >= timeout {
                warn!(
                    "Height {stuck} still unservable after {}s despite being within a node's \
                     retention window; skipping this single block. Any deposit in it will be \
                     recovered by the balance-poll sweep",
                    elapsed.as_secs()
                );
                counter!("relayer_scan_blocks_skipped_total", "reason" => "unservable")
                    .increment(1);
                advance_cursor(cursor, stuck, store);
                *pruned = None;
            } else {
                warn!(
                    "Height {stuck} is retained but not served by any endpoint; skipping it in \
                     {}s unless it becomes fetchable",
                    timeout.saturating_sub(elapsed).as_secs()
                );
            }
        }
        PrunedVerdict::Wait => {
            debug!(
                "Height {stuck} not provably unrecoverable (pruneded {}s); retrying",
                elapsed.as_secs()
            );
        }
    }
}

/// Advance and persist the scan cursor during pruned recovery (the normal per-block
/// advance lives in `scan_to` and follows the same persist-then-gauge pattern).
fn advance_cursor(cursor: &mut u64, new_cursor: u64, store: &Arc<Mutex<RetryStore>>) {
    *cursor = new_cursor;
    if let Err(e) = store.lock().unwrap().store_height(new_cursor) {
        warn!("Failed to persist scan cursor at height {new_cursor}: {e:#}");
    }
    gauge!("relayer_scan_height").set(new_cursor as f64);
}

async fn scan_session(
    http: &HttpClient,
    ws_url: &str,
    cursor: &mut u64,
    confirmation_depth: u64,
    live: &LiveSet,
    store: &Arc<Mutex<RetryStore>>,
    deposits_tx: &Sender<String>,
) -> Result<()> {
    let (ws, driver) = WebSocketClient::new(ws_url)
        .await
        .with_context(|| format!("Failed to connect WebSocket at {}", redact_userinfo(ws_url)))?;
    let driver_handle = tokio::spawn(async move { driver.run().await });
    let mut subscription = ws
        .subscribe(Query::from(EventType::NewBlock))
        .await
        .context("Failed to subscribe to NewBlock events")?;

    // Catch up to the confirmed tip before processing live events, so a restart with
    // an old cursor replays every intervening block. Stay `confirmation_depth` blocks
    // behind so we never read a height whose tx-results the node hasn't indexed yet.
    let tip = current_height(http).await?;
    scan_to(
        http,
        cursor,
        tip.saturating_sub(confirmation_depth),
        live,
        store,
        deposits_tx,
    )
    .await?;

    while let Some(event) = subscription.next().await {
        let event = event.context("WebSocket subscription error")?;
        if let EventData::NewBlock {
            block: Some(block), ..
        } = event.data
        {
            // A NewBlock for height H only authorizes scanning up to H - depth; the
            // tip itself stays unscanned until depth more blocks confirm it.
            let height = block.header.height.value();
            scan_to(
                http,
                cursor,
                height.saturating_sub(confirmation_depth),
                live,
                store,
                deposits_tx,
            )
            .await?;
        }
    }

    let _ = ws.close();
    let _ = driver_handle.await;
    Ok(())
}

/// Scan every block in `(*cursor, target]`, enqueuing deposits to watched addresses
/// and advancing + persisting the cursor after each block.
async fn scan_to(
    http: &HttpClient,
    cursor: &mut u64,
    target: u64,
    live: &LiveSet,
    store: &Arc<Mutex<RetryStore>>,
    deposits_tx: &Sender<String>,
) -> Result<()> {
    while *cursor < target {
        let height = *cursor + 1;
        let block_height = Height::try_from(height).context("Block height out of range")?;
        let results = http
            .block_results(block_height)
            .await
            .with_context(|| format!("Failed to fetch block_results for height {height}"))?;

        // Collect the watched recipients while holding the lock, then release it
        // before sending (the bounded channel's send is async and must not be
        // awaited while holding the std mutex).
        let matched: Vec<String> = {
            let live = live.lock().unwrap();
            extract_deposits(&results)
                .into_iter()
                .filter(|deposit| live.contains_key(&deposit.recipient))
                .map(|deposit| deposit.recipient)
                .collect()
        };
        for recipient in matched {
            counter!("relayer_deposits_detected_total").increment(1);
            debug!("Deposit detected at height {height} for watched address {recipient}");
            // Awaiting here applies backpressure: if the channel is full the scanner
            // pauses (and the cursor isn't advanced past this block) until the
            // dispatcher drains, rather than buffering without bound. A closed
            // receiver only happens on shutdown.
            let _ = deposits_tx.send(recipient).await;
        }

        *cursor = height;
        if let Err(e) = store.lock().unwrap().store_height(height) {
            warn!("Failed to persist scan cursor at height {height}: {e:#}");
        }
        gauge!("relayer_scan_height").set(height as f64);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tendermint::abci::Event;

    fn transfer(recipient: &str, amount: &str) -> Event {
        Event::new(
            "transfer",
            vec![
                ("sender", "celestia1sender"),
                ("recipient", recipient),
                ("amount", amount),
            ],
        )
    }

    fn coin_received(receiver: &str, amount: &str) -> Event {
        Event::new(
            "coin_received",
            vec![("receiver", receiver), ("amount", amount)],
        )
    }

    #[test]
    fn parses_transfer_and_coin_received() {
        let events = [transfer("celestia1aaa", "1000utia")];
        let deposits = deposits_from_events(events.iter());
        assert_eq!(deposits.len(), 1);
        assert_eq!(deposits[0].recipient, "celestia1aaa");
        assert_eq!(
            deposits[0].coins,
            vec![Balance {
                denom: "utia".into(),
                amount: "1000".into()
            }]
        );

        let events = [coin_received("celestia1bbb", "42utia")];
        let deposits = deposits_from_events(events.iter());
        assert_eq!(deposits.len(), 1);
        assert_eq!(deposits[0].recipient, "celestia1bbb");
    }

    #[test]
    fn dedupes_recipient_across_paired_events() {
        // A single bank send emits both coin_received and transfer for the same
        // recipient; we must surface only one deposit.
        let events = [
            coin_received("celestia1aaa", "1000utia"),
            transfer("celestia1aaa", "1000utia"),
        ];
        let deposits = deposits_from_events(events.iter());
        assert_eq!(deposits.len(), 1);
        assert_eq!(deposits[0].recipient, "celestia1aaa");
    }

    #[test]
    fn parses_multiple_coins() {
        let events = [transfer("celestia1aaa", "1000utia,500uatom")];
        let deposits = deposits_from_events(events.iter());
        assert_eq!(
            deposits[0].coins,
            vec![
                Balance {
                    denom: "utia".into(),
                    amount: "1000".into()
                },
                Balance {
                    denom: "uatom".into(),
                    amount: "500".into()
                },
            ]
        );
    }

    #[test]
    fn ignores_unrelated_events_and_keeps_all_recipients() {
        let events = [
            Event::new(
                "message",
                vec![("action", "/celestia.forwarding.v1.MsgForward")],
            ),
            coin_received("celestia1aaa", "1utia"),
            transfer("celestia1ccc", "2utia"),
        ];
        let deposits = deposits_from_events(events.iter());
        let recipients: Vec<_> = deposits.iter().map(|d| d.recipient.as_str()).collect();
        assert_eq!(recipients, vec!["celestia1aaa", "celestia1ccc"]);
    }

    #[test]
    fn empty_block_yields_nothing() {
        let deposits = deposits_from_events(std::iter::empty());
        assert!(deposits.is_empty());
    }

    /// Endpoint testimony: retains blocks from `earliest`; `probe` is the outcome
    /// of directly fetching the stuck height (None = not probed).
    fn testimony(earliest: u64, probe: Option<bool>) -> HeightEvidence {
        HeightEvidence { earliest, probe }
    }

    #[test]
    fn waits_when_no_endpoint_answers() {
        // A total outage is connectivity evidence, not height evidence: never skip.
        assert_eq!(assess_evidence(&[], 2, 100), PrunedVerdict::Wait);
    }

    #[test]
    fn waits_when_any_endpoint_serves_the_height() {
        // One endpoint serving the height outweighs everything else, including a
        // broken holder and a pruned neighbor — the next session will fetch it.
        let evidence = [
            testimony(500, None),      // pruned
            testimony(1, Some(false)), // holder, broken
            testimony(1, Some(true)),  // holder, serving
        ];
        assert_eq!(assess_evidence(&evidence, 3, 100), PrunedVerdict::Wait);
    }

    #[test]
    fn waits_for_a_syncing_holder() {
        // The endpoint retains the height but wasn't probed (tip not far enough
        // past it): it is still syncing toward the height, so waiting fixes this.
        let evidence = [testimony(1, None)];
        assert_eq!(assess_evidence(&evidence, 1, 100), PrunedVerdict::Wait);
    }

    #[test]
    fn jumps_when_all_endpoints_pruned_the_height() {
        // Both endpoints answered and both pruned height 100; resume just before
        // the lowest retained height (5000), i.e. new_cursor 4999.
        let evidence = [testimony(5000, None), testimony(6000, None)];
        assert_eq!(
            assess_evidence(&evidence, 2, 100),
            PrunedVerdict::Pruned {
                new_cursor: 4999,
                unanimous: true
            }
        );
    }

    #[test]
    fn pruned_verdict_is_not_unanimous_with_missing_testimony() {
        // One of three endpoints didn't answer; it may still hold the height, so
        // the verdict must not authorize an immediate jump.
        let evidence = [testimony(5000, None), testimony(6000, None)];
        assert_eq!(
            assess_evidence(&evidence, 3, 100),
            PrunedVerdict::Pruned {
                new_cursor: 4999,
                unanimous: false
            }
        );
    }

    #[test]
    fn unservable_when_a_holder_fails_to_serve() {
        // The endpoint retains the height, its tip is well past it, and the direct
        // probe failed: height-specific breakage, skippable (after the timeout).
        let evidence = [testimony(1, Some(false))];
        assert_eq!(
            assess_evidence(&evidence, 1, 100),
            PrunedVerdict::Unservable
        );
    }

    #[test]
    fn mixed_pruned_and_broken_holder_is_unservable_not_pruned() {
        // One endpoint pruned the height but another still holds it (and fails to
        // serve it): the height is not gone from the pool, so this must be the
        // single-block unservable skip, never a multi-block pruned jump.
        let evidence = [testimony(5000, None), testimony(1, Some(false))];
        assert_eq!(
            assess_evidence(&evidence, 2, 100),
            PrunedVerdict::Unservable
        );
    }

    #[test]
    fn redacts_basic_userinfo() {
        // token-as-username and user:pass forms are both stripped, host preserved.
        assert_eq!(
            redact_userinfo("https://tok3n@rpc.example.com:26657"),
            "https://***@rpc.example.com:26657"
        );
        assert_eq!(
            redact_userinfo("https://user:pass@rpc.example.com:26657/path?q=1"),
            "https://***@rpc.example.com:26657/path?q=1"
        );
    }

    #[test]
    fn leaves_credential_free_urls_unchanged() {
        assert_eq!(
            redact_userinfo("http://localhost:26657"),
            "http://localhost:26657"
        );
        // a '@' in the path (not the authority) must not be treated as userinfo.
        assert_eq!(
            redact_userinfo("http://host:26657/a@b"),
            "http://host:26657/a@b"
        );
        assert_eq!(redact_userinfo("not-a-url"), "not-a-url");
    }

    #[test]
    fn ws_url_preserves_userinfo_for_auth() {
        // derive_ws_url must keep credentials so the WS handshake can authenticate.
        assert_eq!(
            derive_ws_url("https://tok@host:26657"),
            "wss://tok@host:26657/websocket"
        );
    }

    #[test]
    fn with_auth_token_splices_userinfo() {
        // Token goes in the password field (empty username) → `:token@host`.
        assert_eq!(
            with_auth_token("https://rpc.example.com:26657", Some("abc123")).unwrap(),
            "https://:abc123@rpc.example.com:26657"
        );
    }

    #[test]
    fn with_auth_token_noop_when_absent() {
        assert_eq!(
            with_auth_token("http://host:26657", None).unwrap(),
            "http://host:26657"
        );
    }

    /// Drive a real `tendermint_rpc::HttpClient` — built exactly as the scanner
    /// builds it, via [`with_auth_token`] — against a throwaway server that records
    /// the `Authorization` header of the request it receives, then returns 401. We
    /// only assert what reached the wire, so a valid RPC response is unnecessary.
    /// This proves the *actual* library emits Basic auth (not just that we format a
    /// URL), and pins the exact header value the e2e auth-proxy must match.
    async fn captured_auth_header(token: Option<&str>) -> Option<String> {
        use std::sync::{Arc, Mutex};
        use tendermint_rpc::Client;

        let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let slot_handler = slot.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let slot_handler = slot_handler.clone();
                async move {
                    // The handler runs before the 401 is flushed, so once the
                    // client's request completes below, this has already written.
                    *slot_handler.lock().unwrap() = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    axum::http::StatusCode::UNAUTHORIZED
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let base = format!("http://127.0.0.1:{}", addr.port());
        let url = with_auth_token(&base, token).unwrap();
        let http = HttpClient::new(url.as_str()).unwrap();
        // Errors (server 401s) — we only care about the header it captured.
        let _ = http.status().await;

        let captured = slot.lock().unwrap().clone();
        server.abort();
        captured
    }

    #[tokio::test]
    async fn rpc_sends_basic_auth_when_token_set() {
        // The token is spliced as the Basic *password* with an empty username
        // (`:token@host`), so the wire credential is base64(":e2e-secret-token").
        // This empty-username form is the one both reqwest and tendermint encode
        // identically, so the server sees a single unambiguous value. It is the
        // exact header docker-compose.auth.yml's nginx proxy requires; if the token
        // changes, that config's expected value must change too.
        let got = captured_auth_header(Some("e2e-secret-token")).await;
        assert_eq!(got.as_deref(), Some("Basic OmUyZS1zZWNyZXQtdG9rZW4="));
    }

    #[tokio::test]
    async fn rpc_sends_no_auth_when_token_absent() {
        let got = captured_auth_header(None).await;
        assert_eq!(got, None);
    }
}
