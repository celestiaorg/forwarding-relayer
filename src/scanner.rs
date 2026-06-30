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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
use crate::{Balance, ForwardingRequest};

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
/// unhealthy node degrades to its neighbor instead of stalling deposit detection.
struct RpcPool {
    endpoints: Vec<RpcEndpoint>,
}

impl RpcPool {
    /// Parse a comma-separated list of RPC URLs (surrounding whitespace trimmed,
    /// empty entries skipped) into a pool, building one HTTP client per endpoint.
    /// A single URL yields a one-endpoint pool, i.e. the original no-failover behavior.
    fn new(spec: &str) -> Result<Self> {
        let endpoints = spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|url| {
                Ok(RpcEndpoint {
                    url: url.to_string(),
                    http: HttpClient::new(url)
                        .with_context(|| format!("Invalid CometBFT RPC URL: {url}"))?,
                    ws_url: derive_ws_url(url),
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
/// fails over to its neighbor. Because the cursor only advances on success, failing
/// over mid-stream never skips a block — the next endpoint resumes where this one left off.
pub(crate) async fn run_block_scanner(
    rpc_url: String,
    start_height: Option<u64>,
    confirmation_depth: u64,
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
            Ok(()) => warn!(
                "Block subscription ended on {}; reconnecting in {}s",
                endpoint.url,
                RECONNECT_DELAY.as_secs()
            ),
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
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
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
        .with_context(|| format!("Failed to connect WebSocket at {ws_url}"))?;
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
}
