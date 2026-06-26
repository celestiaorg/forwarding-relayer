//! Per-IP request rate limiting for the backend submission endpoint.
//!
//! Every client IP gets a default per-minute budget. A configurable whitelist of
//! "Apps" (each a set of IPs and/or hostnames) is granted a higher per-minute
//! budget. Hostnames are resolved to IPs once at startup, so matching at request
//! time is a pure IP lookup.
//!
//! The limiter uses a token bucket per IP: the budget is the bucket capacity and
//! tokens refill continuously at `budget / 60` per second. This smooths bursts and
//! avoids the fixed-window boundary-doubling problem.

use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

/// Length of the rate-limiting window. Budgets are expressed per this window.
const WINDOW: Duration = Duration::from_secs(60);

/// Default budget for any IP not on the whitelist: one request per minute.
const DEFAULT_PER_MINUTE: u32 = 1;

/// Default budget for whitelisted Apps that do not set their own: 6000/min (100/s).
const DEFAULT_WHITELIST_PER_MINUTE: u32 = 6000;

fn default_default_per_minute() -> u32 {
    DEFAULT_PER_MINUTE
}

fn default_whitelist_per_minute() -> u32 {
    DEFAULT_WHITELIST_PER_MINUTE
}

/// On-disk rate-limit configuration (JSON).
///
/// ```json
/// {
///   "default_per_minute": 1,
///   "whitelist_default_per_minute": 6000,
///   "apps": [
///     { "name": "my-app", "hosts": ["203.0.113.10", "api.example.com"], "per_minute": 6000 }
///   ]
/// }
/// ```
///
/// A budget of `0` means unlimited (no limiting for that IP).
#[derive(Debug, Deserialize)]
pub struct RateLimitFileConfig {
    /// Per-minute budget for IPs that are not whitelisted.
    #[serde(default = "default_default_per_minute")]
    pub default_per_minute: u32,
    /// Per-minute budget applied to whitelisted Apps that omit their own `per_minute`.
    #[serde(default = "default_whitelist_per_minute")]
    pub whitelist_default_per_minute: u32,
    /// Whitelisted Apps.
    #[serde(default)]
    pub apps: Vec<AppConfig>,
}

/// A single whitelisted App: a set of hosts that share an elevated budget.
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    /// Human-readable label, used in logs.
    pub name: String,
    /// IP addresses or hostnames belonging to the App. Hostnames are resolved at startup.
    pub hosts: Vec<String>,
    /// Per-minute budget for each of this App's IPs. Falls back to
    /// `whitelist_default_per_minute` when unset.
    pub per_minute: Option<u32>,
}

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitDecision {
    /// The request is within budget and may proceed.
    Allowed,
    /// The request exceeds the budget. `retry_after_secs` is a hint for when a
    /// token will next be available.
    Limited { retry_after_secs: u64 },
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Per-IP token-bucket rate limiter.
pub struct RateLimiter {
    /// Budget for IPs without an explicit override.
    default_per_minute: u32,
    /// Per-IP budget overrides for whitelisted hosts.
    overrides: HashMap<IpAddr, u32>,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

impl RateLimiter {
    /// Load a limiter from a JSON config file, resolving hostnames to IPs.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read rate-limit config at {:?}", path))?;
        let config: RateLimitFileConfig = serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse rate-limit config at {:?}", path))?;
        Ok(Self::from_config(config))
    }

    /// Build a limiter from parsed config, resolving each App's hostnames to IPs.
    ///
    /// Resolution failures are logged and skipped rather than aborting startup; an
    /// unresolved host simply falls back to the default budget.
    pub fn from_config(config: RateLimitFileConfig) -> Self {
        let mut overrides: HashMap<IpAddr, u32> = HashMap::new();

        for app in &config.apps {
            let budget = app.per_minute.unwrap_or(config.whitelist_default_per_minute);
            for host in &app.hosts {
                match resolve_host(host) {
                    Ok(ips) if ips.is_empty() => {
                        warn!(app = %app.name, host = %host, "Rate-limit host resolved to no addresses; skipping");
                    }
                    Ok(ips) => {
                        for ip in ips {
                            if let Some(existing) = overrides.insert(ip, budget) {
                                if existing != budget {
                                    warn!(
                                        app = %app.name, %ip, existing, new = budget,
                                        "Rate-limit IP listed by multiple Apps with different budgets; using the later one"
                                    );
                                }
                            }
                            info!(app = %app.name, %ip, per_minute = budget, "Whitelisted rate-limit host");
                        }
                    }
                    Err(err) => {
                        warn!(app = %app.name, host = %host, "Failed to resolve rate-limit host: {err:#}; it will use the default budget");
                    }
                }
            }
        }

        info!(
            default_per_minute = config.default_per_minute,
            whitelisted_ips = overrides.len(),
            "Rate limiting enabled"
        );

        Self {
            default_per_minute: config.default_per_minute,
            overrides,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// The per-minute budget that applies to `ip`.
    fn budget_for(&self, ip: IpAddr) -> u32 {
        self.overrides
            .get(&ip)
            .copied()
            .unwrap_or(self.default_per_minute)
    }

    /// Check whether a request from `ip` is allowed, using the current time.
    pub fn check(&self, ip: IpAddr) -> RateLimitDecision {
        self.check_at(ip, Instant::now())
    }

    /// Check a request from `ip` at an explicit instant (used in tests).
    fn check_at(&self, ip: IpAddr, now: Instant) -> RateLimitDecision {
        let budget = self.budget_for(ip);
        if budget == 0 {
            return RateLimitDecision::Allowed;
        }
        let capacity = budget as f64;
        let rate = capacity / WINDOW.as_secs_f64();

        let mut buckets = self.buckets.lock().unwrap();
        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: capacity,
            last_refill: now,
        });

        let elapsed = now.saturating_duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            RateLimitDecision::Allowed
        } else {
            let needed = 1.0 - bucket.tokens;
            let retry_after_secs = (needed / rate).ceil() as u64;
            RateLimitDecision::Limited {
                retry_after_secs: retry_after_secs.max(1),
            }
        }
    }

    /// Drop buckets that have fully replenished, bounding memory under churn from
    /// many distinct (e.g. unbounded, non-whitelisted) IPs. A dropped bucket is
    /// indistinguishable from a freshly created full one.
    pub fn cleanup(&self) {
        self.cleanup_at(Instant::now());
    }

    fn cleanup_at(&self, now: Instant) {
        let mut buckets = self.buckets.lock().unwrap();
        buckets.retain(|ip, bucket| {
            let budget = self.budget_for(*ip);
            if budget == 0 {
                return false;
            }
            let capacity = budget as f64;
            let rate = capacity / WINDOW.as_secs_f64();
            let elapsed = now.saturating_duration_since(bucket.last_refill).as_secs_f64();
            let tokens = (bucket.tokens + elapsed * rate).min(capacity);
            // Keep only buckets that still carry a deficit; full buckets are
            // safe to forget.
            tokens < capacity
        });
    }
}

/// Resolve a host string (IP literal or hostname) to a list of IPs.
fn resolve_host(host: &str) -> Result<Vec<IpAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![ip]);
    }
    // Port is irrelevant; we only want the resolved addresses.
    let addrs = (host, 0u16)
        .to_socket_addrs()
        .with_context(|| format!("DNS resolution failed for {host}"))?;
    Ok(addrs.map(|sock| sock.ip()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    fn limiter(default_per_minute: u32, overrides: Vec<(IpAddr, u32)>) -> RateLimiter {
        RateLimiter {
            default_per_minute,
            overrides: overrides.into_iter().collect(),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn default_allows_one_per_minute() {
        let rl = limiter(1, vec![]);
        let t0 = Instant::now();
        assert_eq!(rl.check_at(ip(1), t0), RateLimitDecision::Allowed);
        // Second request within the same minute is limited.
        assert!(matches!(
            rl.check_at(ip(1), t0 + Duration::from_secs(1)),
            RateLimitDecision::Limited { .. }
        ));
    }

    #[test]
    fn default_refills_after_window() {
        let rl = limiter(1, vec![]);
        let t0 = Instant::now();
        assert_eq!(rl.check_at(ip(1), t0), RateLimitDecision::Allowed);
        assert!(matches!(
            rl.check_at(ip(1), t0 + Duration::from_secs(30)),
            RateLimitDecision::Limited { .. }
        ));
        // After a full window the bucket has refilled to one token.
        assert_eq!(
            rl.check_at(ip(1), t0 + Duration::from_secs(60)),
            RateLimitDecision::Allowed
        );
    }

    #[test]
    fn retry_after_hint_is_reasonable() {
        let rl = limiter(1, vec![]);
        let t0 = Instant::now();
        assert_eq!(rl.check_at(ip(1), t0), RateLimitDecision::Allowed);
        // Immediately after exhausting, a full window is needed for the next token.
        match rl.check_at(ip(1), t0) {
            RateLimitDecision::Limited { retry_after_secs } => {
                assert!((1..=60).contains(&retry_after_secs), "got {retry_after_secs}");
            }
            other => panic!("expected limited, got {other:?}"),
        }
    }

    #[test]
    fn whitelisted_ip_gets_higher_budget() {
        let rl = limiter(1, vec![(ip(7), 6000)]);
        let t0 = Instant::now();
        // Burst of 6000 is allowed immediately for the whitelisted IP.
        for i in 0..6000 {
            assert_eq!(
                rl.check_at(ip(7), t0),
                RateLimitDecision::Allowed,
                "request {i} should be allowed"
            );
        }
        // The 6001st within the same instant is limited.
        assert!(matches!(
            rl.check_at(ip(7), t0),
            RateLimitDecision::Limited { .. }
        ));
        // A non-whitelisted IP still gets the default budget.
        assert_eq!(rl.check_at(ip(8), t0), RateLimitDecision::Allowed);
        assert!(matches!(
            rl.check_at(ip(8), t0),
            RateLimitDecision::Limited { .. }
        ));
    }

    #[test]
    fn budget_zero_means_unlimited() {
        let rl = limiter(0, vec![]);
        let t0 = Instant::now();
        for _ in 0..1000 {
            assert_eq!(rl.check_at(ip(1), t0), RateLimitDecision::Allowed);
        }
    }

    #[test]
    fn cleanup_drops_replenished_buckets_only() {
        let rl = limiter(1, vec![]);
        let t0 = Instant::now();
        // ip(1) exhausts its bucket (deficit), ip(2) we leave untouched after one use.
        assert_eq!(rl.check_at(ip(1), t0), RateLimitDecision::Allowed);
        assert_eq!(rl.check_at(ip(2), t0), RateLimitDecision::Allowed);

        // Shortly after: both still carry a deficit, so both are retained.
        rl.cleanup_at(t0 + Duration::from_secs(1));
        assert_eq!(rl.buckets.lock().unwrap().len(), 2);

        // After a full window both have refilled and are dropped.
        rl.cleanup_at(t0 + Duration::from_secs(61));
        assert_eq!(rl.buckets.lock().unwrap().len(), 0);
    }

    #[test]
    fn from_config_resolves_ip_literals() {
        let config = RateLimitFileConfig {
            default_per_minute: 1,
            whitelist_default_per_minute: 6000,
            apps: vec![AppConfig {
                name: "app".to_string(),
                hosts: vec!["10.0.0.7".to_string()],
                per_minute: None,
            }],
        };
        let rl = RateLimiter::from_config(config);
        assert_eq!(rl.budget_for(ip(7)), 6000);
        assert_eq!(rl.budget_for(ip(8)), 1);
    }
}
