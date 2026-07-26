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

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::RwLock;
use std::time::Duration;

use di_core::RequestScope;
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
        if state.seen % 4096 == 0 {
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
