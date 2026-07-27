//! Rate limiting — a per-client token bucket that returns `429 Too Many Requests`.
//!
//! Protects against floods and brute-force by capping how fast one client may hit the
//! server. A **token bucket** allows short bursts (up to the capacity) while holding
//! the *sustained* rate to the refill rate — friendlier than a hard fixed window, and
//! smooth across window boundaries. The client is keyed by its real IP, resolved the
//! same proxy-aware way as the ban list (trust `X-Forwarded-For` only from a
//! configured proxy).
//!
//! ```rust,ignore
//! RateLimit::new(100, Duration::from_secs(60))  // 100 req/min sustained, burst 100
//!     .burst(20)                                 // optionally a smaller burst
//!     .trust_proxy(proxy_ip)
//! ```
//!
//! ## Reasonable limits (per-IP, sustained) — a reference
//!
//! Starting points, not laws; measure your own traffic. The named presets below
//! ([`RateLimit::for_login`] etc.) encode this table so it is usable, not just prose.
//!
//! | Endpoint kind        | Sustained   | Burst | Notes                                        |
//! |----------------------|-------------|-------|----------------------------------------------|
//! | Login / auth         | 5–10 /min   | 3–5   | brute-force sensitive; pair with `LoginGuard` |
//! | Web page (HTML)      | 60–120 /min | 20–40 | humans don't click faster                    |
//! | API read (authed)    | 60–120 /min | 20–50 | prefer keying by **user**, not IP            |
//! | API write / mutation | 10–30 /min  | 5–10  | stricter                                     |
//! | Public / search API  | 100–300 /min| 50    | by IP                                        |
//! | Static assets        | very high / none |  | usually left to a CDN                     |
//! | Catch-all per-IP     | ~100 /min   | 200   | a sane baseline                              |
//!
//! Caveats: behind a CDN/proxy call [`trust_proxy`](RateLimit::trust_proxy) or you
//! limit the proxy, not the client; NAT/mobile means many users share one IP, so a
//! low per-IP cap blocks legitimate traffic (key by user for authenticated routes);
//! bursts must be generous — one page fires many parallel requests. Set
//! [`ban_after`](RateLimit::ban_after) high (≈20–50), for persistent abusers only.
//!
//! ## From config
//!
//! [`RateLimit::from_config`] reads the numbers from `kernway.properties`, with the
//! `DEFAULT_REQUESTS`/`DEFAULT_PERIOD_SECS` defaults, so ops can tune limits
//! without a rebuild:
//!
//! ```properties
//! kernway.ratelimit.requests        = 100     # sustained requests per period
//! kernway.ratelimit.period-secs     = 60
//! kernway.ratelimit.burst           = 200     # optional; defaults to `requests`
//! kernway.ratelimit.ban-after       = 30      # optional; throttles before an auto-ban
//! kernway.ratelimit.trusted-proxies = 10.0.0.1,10.0.0.2
//! ```

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::RwLock;
use std::time::Duration;

use di_core::RequestScope;
use kernway_config::Config;
use kernway_core::error::StatusCode;
use kernway_core::layer::BoxFuture;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_security::tracking::client_ip;
use kernway_security::Bans;

use crate::middleware::{Middleware, Next};

const FORWARDED_FOR: &str = "x-forwarded-for";

/// Drop idle buckets not seen for this long, to bound memory (swept occasionally).
const IDLE_EVICT_MS: u64 = 300_000; // 5 minutes

/// Default sustained request count for [`RateLimit::from_config`] (the catch-all
/// baseline from the reference table).
pub const DEFAULT_REQUESTS: u32 = 100;
/// Default period (seconds) for [`RateLimit::from_config`].
pub const DEFAULT_PERIOD_SECS: u64 = 60;

/// One client's bucket: how many tokens are left, when it was last refilled, and how
/// many times it has been throttled (for the auto-ban escalation).
struct Bucket {
    tokens: f64,
    last_ms: u64,
    violations: u32,
}

struct State {
    buckets: HashMap<IpAddr, Bucket>,
    seen: u64,
}

/// A per-IP token-bucket rate limiter. Requests over the rate get `429` with a
/// `Retry-After`.
pub struct RateLimit {
    capacity: f64,
    refill_per_sec: f64,
    trusted: Vec<IpAddr>,
    state: RwLock<State>,
    /// Optional escalation: `(violations, ban list)` — after this many throttled
    /// requests, add the client IP to the ban list so it is blocked outright.
    ban: Option<(u32, Bans)>,
}

impl RateLimit {
    /// Allow `max_requests` per `period` sustained, with a burst capacity equal to
    /// `max_requests`. E.g. `new(100, 60s)` is 100/min, bursting up to 100.
    #[must_use]
    pub fn new(max_requests: u32, period: Duration) -> Self {
        let capacity = f64::from(max_requests.max(1));
        let refill_per_sec = capacity / period.as_secs_f64().max(0.001);
        Self {
            capacity,
            refill_per_sec,
            trusted: Vec::new(),
            state: RwLock::new(State { buckets: HashMap::new(), seen: 0 }),
            ban: None,
        }
    }

    /// Escalate to an outright **ban**: after a client is throttled `violations` times,
    /// add its IP to `bans` (the same list a `BanFilter` enforces), so it is blocked
    /// rather than merely slowed. This is the "set a number and just ban them" knob —
    /// e.g. `.ban_after(20, bans)` bans an IP that ignores the limit 20 times. Give it
    /// the same `Bans` handle you gave the `BanFilter`; if that is a durable backend,
    /// register the auto-ban through it to persist (this in-memory path blocks
    /// immediately regardless).
    #[must_use]
    pub fn ban_after(mut self, violations: u32, bans: Bans) -> Self {
        self.ban = Some((violations.max(1), bans));
        self
    }

    // Named presets encoding the reference table in the module docs — a usable,
    // greppable record of "reasonable per-IP limits", not just prose.

    /// Login / auth endpoints: 10/min, burst 5 — brute-force sensitive. Pair with a
    /// [`LoginGuard`](kernway_security::LoginGuard).
    #[must_use]
    pub fn for_login() -> Self {
        Self::new(10, Duration::from_secs(60)).burst(5)
    }

    /// HTML web pages: 120/min, burst 40 — humans don't navigate faster.
    #[must_use]
    pub fn for_web() -> Self {
        Self::new(120, Duration::from_secs(60)).burst(40)
    }

    /// Read-heavy API: 120/min, burst 50. Consider keying by user for authed routes.
    #[must_use]
    pub fn for_api() -> Self {
        Self::new(120, Duration::from_secs(60)).burst(50)
    }

    /// Write / mutation endpoints: 30/min, burst 10 — stricter than reads.
    #[must_use]
    pub fn for_writes() -> Self {
        Self::new(30, Duration::from_secs(60)).burst(10)
    }

    /// Public / search API: 300/min, burst 50.
    #[must_use]
    pub fn for_public() -> Self {
        Self::new(300, Duration::from_secs(60)).burst(50)
    }

    /// Build from `kernway.ratelimit.*` config (see the module docs), falling back to
    /// `DEFAULT_REQUESTS`/`DEFAULT_PERIOD_SECS`. Reads `burst` and
    /// `trusted-proxies` if present; auto-ban is only wired by
    /// [`from_config_with_bans`](Self::from_config_with_bans) (it needs a `Bans`).
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let requests = config.get_or("kernway.ratelimit.requests", DEFAULT_REQUESTS);
        let period = config.get_or("kernway.ratelimit.period-secs", DEFAULT_PERIOD_SECS);
        let mut limiter = Self::new(requests, Duration::from_secs(period));
        if let Some(burst) = config.get::<u32>("kernway.ratelimit.burst") {
            limiter = limiter.burst(burst);
        }
        for proxy in config.get_str("kernway.ratelimit.trusted-proxies").unwrap_or("").split(',') {
            if let Ok(ip) = proxy.trim().parse::<IpAddr>() {
                limiter.trusted.push(ip);
            }
        }
        limiter
    }

    /// [`from_config`](Self::from_config) plus the auto-ban escalation from
    /// `kernway.ratelimit.ban-after`, using `bans` (the list a `BanFilter` enforces).
    #[must_use]
    pub fn from_config_with_bans(config: &Config, bans: Bans) -> Self {
        let mut limiter = Self::from_config(config);
        if let Some(after) = config.get::<u32>("kernway.ratelimit.ban-after") {
            limiter = limiter.ban_after(after, bans);
        }
        limiter
    }

    /// Override the burst capacity (the default equals `max_requests`). A smaller
    /// burst smooths traffic; a larger one tolerates spikes.
    #[must_use]
    pub fn burst(mut self, capacity: u32) -> Self {
        self.capacity = f64::from(capacity.max(1));
        self
    }

    /// Trust a reverse proxy by IP, so the real client IP is read from
    /// `X-Forwarded-For` (and one client behind it is not the whole proxy's traffic).
    #[must_use]
    pub fn trust_proxy(mut self, ip: IpAddr) -> Self {
        self.trusted.push(ip);
        self
    }

    /// Whether a request from `ip` is allowed at `now_ms`, consuming a token if so.
    /// The core logic, taking the clock explicitly so it is deterministically testable.
    fn allow_at(&self, ip: IpAddr, now_ms: u64) -> bool {
        let mut state = self.state.write().unwrap();

        // Occasionally evict idle buckets so the map does not grow without bound.
        state.seen = state.seen.wrapping_add(1);
        if state.seen.is_multiple_of(4096) {
            state.buckets.retain(|_, b| now_ms.saturating_sub(b.last_ms) < IDLE_EVICT_MS);
        }

        let capacity = self.capacity;
        let refill = self.refill_per_sec;
        let bucket = state.buckets.entry(ip).or_insert(Bucket { tokens: capacity, last_ms: now_ms, violations: 0 });

        // Refill for the elapsed time, capped at capacity.
        let elapsed_secs = now_ms.saturating_sub(bucket.last_ms) as f64 / 1000.0;
        bucket.tokens = (bucket.tokens + elapsed_secs * refill).min(capacity);
        bucket.last_ms = now_ms;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return true;
        }

        // Throttled. If escalation is configured, count it and ban at the threshold.
        if let Some((threshold, bans)) = &self.ban {
            bucket.violations += 1;
            if bucket.violations >= *threshold {
                bans.ban_ip(ip); // immediate in-memory ban (a BanFilter enforces it)
            }
        }
        false
    }
}

/// The `429` a throttled request receives, with a `Retry-After` of one refill period.
fn too_many(retry_after_secs: u64) -> Response {
    let mut response = Response::new(StatusCode::TOO_MANY_REQUESTS)
        .content_type("application/json; charset=utf-8")
        .body(br#"{"status":429,"title":"Too Many Requests","detail":"rate limit exceeded"}"#.to_vec());
    response.headers.insert("retry-after", &retry_after_secs.to_string());
    response
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl Middleware for RateLimit {
    fn name(&self) -> &'static str {
        "RateLimit"
    }

    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        // A request we cannot attribute to an IP is allowed (fail open) rather than
        // penalising every unidentifiable client.
        if let Some(ip) = client_ip(req.remote_addr, req.header(FORWARDED_FOR), &self.trusted) {
            if !self.allow_at(ip, now_ms()) {
                let retry = (1.0 / self.refill_per_sec).ceil().max(1.0) as u64;
                let response = too_many(retry);
                return Box::pin(async move { response });
            }
        }
        next.run(req, scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn allows_up_to_the_burst_then_denies() {
        // 5 tokens, refilling slowly; the 6th request in the same instant is denied.
        let rl = RateLimit::new(5, Duration::from_secs(60));
        let client = ip("203.0.113.1");
        for n in 1..=5 {
            assert!(rl.allow_at(client, 1000), "request {n} within burst");
        }
        assert!(!rl.allow_at(client, 1000), "the 6th in the same instant is throttled");
    }

    #[test]
    fn refills_over_time() {
        // 60/min = 1 token/sec. Drain, then a second later exactly one is back.
        let rl = RateLimit::new(60, Duration::from_secs(60));
        let client = ip("203.0.113.2");
        for _ in 0..60 {
            assert!(rl.allow_at(client, 0));
        }
        assert!(!rl.allow_at(client, 0), "drained");
        assert!(rl.allow_at(client, 1000), "one token refilled after 1s");
        assert!(!rl.allow_at(client, 1000), "but only one");
    }

    #[test]
    fn separate_clients_have_separate_buckets() {
        let rl = RateLimit::new(1, Duration::from_secs(60));
        assert!(rl.allow_at(ip("10.0.0.1"), 0));
        assert!(!rl.allow_at(ip("10.0.0.1"), 0), "first client throttled");
        assert!(rl.allow_at(ip("10.0.0.2"), 0), "a different client is unaffected");
    }

    #[test]
    fn burst_can_be_set_below_the_rate() {
        // 100/min sustained but only 3 in a burst.
        let rl = RateLimit::new(100, Duration::from_secs(60)).burst(3);
        let client = ip("10.0.0.3");
        for _ in 0..3 {
            assert!(rl.allow_at(client, 0));
        }
        assert!(!rl.allow_at(client, 0), "burst capped at 3 despite the higher rate");
    }

    fn config(props: &str) -> Config {
        kernway_config::ConfigBuilder::default().parse(props).build()
    }

    #[test]
    fn from_config_reads_values_and_falls_back_to_defaults() {
        // Defaults when nothing is set.
        let d = RateLimit::from_config(&config(""));
        assert_eq!(d.capacity, f64::from(DEFAULT_REQUESTS));
        // Explicit values, including a burst override and a trusted proxy.
        let rl = RateLimit::from_config(&config(
            "kernway.ratelimit.requests=50\nkernway.ratelimit.period-secs=10\nkernway.ratelimit.burst=5\nkernway.ratelimit.trusted-proxies=10.0.0.1",
        ));
        assert_eq!(rl.capacity, 5.0, "burst override");
        assert!((rl.refill_per_sec - 5.0).abs() < 1e-9, "50 per 10s = 5/s");
        assert_eq!(rl.trusted, vec![ip("10.0.0.1")]);
    }

    #[test]
    fn from_config_with_bans_wires_auto_ban() {
        let bans = Bans::new();
        let rl = RateLimit::from_config_with_bans(
            &config("kernway.ratelimit.requests=1\nkernway.ratelimit.period-secs=3600\nkernway.ratelimit.ban-after=2"),
            bans.clone(),
        );
        let client = ip("203.0.113.20");
        assert!(rl.allow_at(client, 0));
        rl.allow_at(client, 0); // violation 1
        rl.allow_at(client, 0); // violation 2 → ban
        assert!(bans.is_banned(Some(client), None), "ban-after from config took effect");
    }

    #[test]
    fn a_login_preset_is_strict() {
        let rl = RateLimit::for_login(); // 10/min, burst 5
        let client = ip("10.0.0.9");
        for _ in 0..5 {
            assert!(rl.allow_at(client, 0));
        }
        assert!(!rl.allow_at(client, 0), "login preset caps the burst at 5");
    }

    #[test]
    fn escalates_to_a_ban_after_repeated_violations() {
        let bans = Bans::new();
        // 1 token, then ban after being throttled 3 times.
        let rl = RateLimit::new(1, Duration::from_secs(3600)).ban_after(3, bans.clone());
        let client = ip("203.0.113.9");

        assert!(rl.allow_at(client, 0), "first request uses the single token");
        assert!(!bans.is_banned(Some(client), None), "not banned yet");

        // Three throttled requests reach the ban threshold.
        for _ in 0..3 {
            assert!(!rl.allow_at(client, 0), "throttled");
        }
        assert!(bans.is_banned(Some(client), None), "the flooding IP is now banned");
        // A well-behaved client never trips it.
        assert!(!bans.is_banned(Some(ip("203.0.113.10")), None));
    }
}
