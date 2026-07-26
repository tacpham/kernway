//! Request tracking — who/where/what of a request, for visitor identity and
//! activity: a stable **visitor id** (even for anonymous guests), the **client IP**
//! resolved correctly behind a reverse proxy, the **User-Agent**, and the **path**.
//!
//! Put in the request scope (KEP-0005) by a `VisitorTracking` middleware, so a
//! handler — an htmx fragment, a kernleaf page, a JSON API — reads it the same way
//! it reads a `SecurityContext`. Tracking is orthogonal to rendering.
//!
//! ## The reverse-proxy trap
//!
//! Behind `client → nginx → kernway`, the socket peer is *nginx*, not the client;
//! the real client is in `X-Forwarded-For`. But that header is **client-settable**,
//! so trusting it blindly lets anyone spoof any IP (bypassing rate limits, allow-
//! lists, geo, logs). [`client_ip`] trusts it **only** when the peer is a configured
//! trusted proxy, and then takes the first *untrusted* address walking the header
//! right-to-left — the address a spoofer cannot push past the real proxy hops.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};

use crate::csrf::CsrfToken;

/// The `kw_visitor` cookie carrying the visitor id.
pub const VISITOR_COOKIE: &str = "kw_visitor";

/// Per-request identity + activity metadata, set in the scope by the tracking
/// middleware. The `user` (a logged-in principal) is a separate concern — the
/// `SecurityContext`; combine them (`user` if authenticated, else `visitor_id`) to
/// key presence.
#[derive(Debug, Clone)]
pub struct RequestMeta {
    /// Stable id for this browser/flow (from `kw_visitor`), present for anonymous
    /// visitors too.
    pub visitor_id: String,
    /// The resolved client IP (proxy-aware), or `None` if unknown.
    pub ip: Option<IpAddr>,
    /// The `User-Agent`, if the request carried one.
    pub user_agent: Option<String>,
    /// The request path (`/checkout`) — where the visitor is right now.
    pub path: String,
    /// The HTTP method.
    pub method: String,
}

/// A fresh random visitor id (32 bytes of OS randomness, hex — the CSRF generator).
#[must_use]
pub fn new_visitor_id() -> String {
    CsrfToken::generate().as_str().to_string()
}

/// The `Set-Cookie` value for a visitor id — `HttpOnly`, `SameSite=Lax`, one year.
#[must_use]
pub fn visitor_cookie(id: &str) -> String {
    format!("{VISITOR_COOKIE}={id}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000")
}

/// Pull the visitor id out of a `Cookie` header value.
#[must_use]
pub fn visitor_from_cookie(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == VISITOR_COOKIE).then(|| value.trim())
    })
}

/// Resolve the real client IP, safely behind a reverse proxy.
///
/// - If the socket peer is **not** a trusted proxy, the request arrived directly (or
///   from an untrusted hop): use the peer, and **ignore** `X-Forwarded-For` (it is
///   spoofable).
/// - If the peer **is** trusted, walk `X-Forwarded-For` right-to-left, skipping
///   trusted proxies; the first untrusted address is the client (a spoofer's fake
///   left-hand entries are shadowed by the real hops on the right). If every entry
///   is trusted (or there is none), fall back to the peer.
#[must_use]
pub fn client_ip(remote_addr: Option<SocketAddr>, forwarded_for: Option<&str>, trusted: &[IpAddr]) -> Option<IpAddr> {
    let peer = remote_addr?.ip();
    if !trusted.contains(&peer) {
        return Some(peer);
    }
    if let Some(forwarded) = forwarded_for {
        for entry in forwarded.split(',').rev() {
            if let Ok(ip) = entry.trim().parse::<IpAddr>() {
                if !trusted.contains(&ip) {
                    return Some(ip);
                }
            }
        }
    }
    Some(peer)
}

// --- Ban list: block a request by IP, subnet, or User-Agent ------------------

/// A CIDR block (`1.2.3.0/24`) — a network address and a prefix length.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cidr {
    network: IpAddr,
    prefix: u8,
}

/// One ban rule.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BanRule {
    Ip(IpAddr),
    Subnet(Cidr),
    UserAgentExact(String),
    UserAgentContains(String), // stored lowercased; matched case-insensitively
}

/// A blocklist matched against a request's resolved IP and User-Agent. Ban an
/// address, a whole subnet, an exact agent, or any agent containing a phrase (a
/// crude bot/scraper filter). Enforced early by a `BanFilter` middleware.
///
/// ```
/// use kernway_security::tracking::BanList;
/// let bans = BanList::new()
///     .ip("1.2.3.4".parse().unwrap())
///     .subnet("10.0.0.0/8")
///     .user_agent_exact("BadBot/1.0")
///     .user_agent_containing("scraper");
/// assert!(bans.is_banned("10.5.6.7".parse().ok(), None));
/// assert!(bans.is_banned(None, Some("Evil Scraper 2.0")));
/// ```
#[derive(Debug, Clone, Default)]
pub struct BanList {
    rules: Vec<BanRule>,
}

impl BanList {
    /// An empty ban list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // Builder methods (consume `self`) for constructing a static list — each
    // delegates to the corresponding `add_*` so the rule shape lives in one place.

    /// Ban a single IP address (builder).
    #[must_use]
    pub fn ip(mut self, ip: IpAddr) -> Self {
        self.add_ip(ip);
        self
    }

    /// Ban a subnet in CIDR notation (`1.2.3.0/24`, `10.0.0.0/8`, an IPv6 prefix).
    /// A bare address (no `/`) bans that single host. An unparseable value is
    /// ignored — a malformed rule bans nothing rather than everything.
    #[must_use]
    pub fn subnet(mut self, cidr: &str) -> Self {
        self.add_subnet(cidr);
        self
    }

    /// Ban an exact User-Agent string (builder).
    #[must_use]
    pub fn user_agent_exact(mut self, agent: &str) -> Self {
        self.add_user_agent_exact(agent);
        self
    }

    /// Ban any User-Agent containing `phrase`, case-insensitive (builder).
    #[must_use]
    pub fn user_agent_containing(mut self, phrase: &str) -> Self {
        self.add_user_agent_containing(phrase);
        self
    }

    // Mutable ops (`&mut self`) for runtime ban/unban (behind [`Bans`]).

    /// Add an IP ban.
    pub fn add_ip(&mut self, ip: IpAddr) {
        let rule = BanRule::Ip(ip);
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
        }
    }

    /// Remove an IP ban (unban). No-op if it was not banned.
    pub fn remove_ip(&mut self, ip: IpAddr) {
        self.rules.retain(|r| *r != BanRule::Ip(ip));
    }

    /// Add a subnet ban (ignored if the CIDR is malformed).
    pub fn add_subnet(&mut self, cidr: &str) {
        if let Some(cidr) = parse_cidr(cidr) {
            let rule = BanRule::Subnet(cidr);
            if !self.rules.contains(&rule) {
                self.rules.push(rule);
            }
        }
    }

    /// Remove a subnet ban (unban).
    pub fn remove_subnet(&mut self, cidr: &str) {
        if let Some(cidr) = parse_cidr(cidr) {
            self.rules.retain(|r| *r != BanRule::Subnet(cidr.clone()));
        }
    }

    /// Add an exact-User-Agent ban.
    pub fn add_user_agent_exact(&mut self, agent: &str) {
        let rule = BanRule::UserAgentExact(agent.to_string());
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
        }
    }

    /// Add a User-Agent-contains ban (case-insensitive).
    pub fn add_user_agent_containing(&mut self, phrase: &str) {
        let rule = BanRule::UserAgentContains(phrase.to_ascii_lowercase());
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
        }
    }

    /// Remove a User-Agent-contains ban (unban).
    pub fn remove_user_agent_containing(&mut self, phrase: &str) {
        self.rules.retain(|r| *r != BanRule::UserAgentContains(phrase.to_ascii_lowercase()));
    }

    /// Drop every rule.
    pub fn clear(&mut self) {
        self.rules.clear();
    }

    /// Whether a request with this `ip` and `user_agent` is banned by any rule.
    #[must_use]
    pub fn is_banned(&self, ip: Option<IpAddr>, user_agent: Option<&str>) -> bool {
        self.rules.iter().any(|rule| match rule {
            BanRule::Ip(banned) => ip == Some(*banned),
            BanRule::Subnet(cidr) => ip.is_some_and(|ip| cidr_contains(cidr, ip)),
            BanRule::UserAgentExact(exact) => user_agent == Some(exact.as_str()),
            BanRule::UserAgentContains(phrase) => {
                user_agent.is_some_and(|ua| ua.to_ascii_lowercase().contains(phrase.as_str()))
            }
        })
    }

    /// Whether any rule is set (so the middleware can no-op an empty list).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// A **shared, runtime-mutable** ban list — the handle an admin uses to ban and
/// **unban** while the server runs. `Clone` is cheap (an `Arc`); give one clone to
/// the `BanFilter` middleware and register another as a bean so an admin handler can
/// `ban_ip`/`unban_ip`. Reads (per request) take a read lock; ban/unban take a brief
/// write lock — bans change rarely, so the lock is effectively uncontended.
#[derive(Clone, Default)]
pub struct Bans(Arc<RwLock<BanList>>);

impl Bans {
    /// An empty, shared ban list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A shared ban list seeded with a static [`BanList`].
    #[must_use]
    pub fn with(list: BanList) -> Self {
        Self(Arc::new(RwLock::new(list)))
    }

    /// Ban an IP at runtime.
    pub fn ban_ip(&self, ip: IpAddr) {
        self.0.write().unwrap().add_ip(ip);
    }

    /// Unban an IP at runtime.
    pub fn unban_ip(&self, ip: IpAddr) {
        self.0.write().unwrap().remove_ip(ip);
    }

    /// Ban a subnet at runtime.
    pub fn ban_subnet(&self, cidr: &str) {
        self.0.write().unwrap().add_subnet(cidr);
    }

    /// Unban a subnet at runtime.
    pub fn unban_subnet(&self, cidr: &str) {
        self.0.write().unwrap().remove_subnet(cidr);
    }

    /// Ban any User-Agent containing `phrase` (case-insensitive) at runtime.
    pub fn ban_user_agent_containing(&self, phrase: &str) {
        self.0.write().unwrap().add_user_agent_containing(phrase);
    }

    /// Unban a User-Agent-contains rule at runtime.
    pub fn unban_user_agent_containing(&self, phrase: &str) {
        self.0.write().unwrap().remove_user_agent_containing(phrase);
    }

    /// Drop every ban.
    pub fn clear(&self) {
        self.0.write().unwrap().clear();
    }

    /// Whether a request with this `ip`/`user_agent` is currently banned.
    #[must_use]
    pub fn is_banned(&self, ip: Option<IpAddr>, user_agent: Option<&str>) -> bool {
        self.0.read().unwrap().is_banned(ip, user_agent)
    }
}

/// Parse `1.2.3.0/24` (or a bare host) into a [`Cidr`]; `None` if malformed.
fn parse_cidr(spec: &str) -> Option<Cidr> {
    match spec.split_once('/') {
        Some((addr, prefix)) => {
            let network: IpAddr = addr.trim().parse().ok()?;
            let prefix: u8 = prefix.trim().parse().ok()?;
            let max = if network.is_ipv4() { 32 } else { 128 };
            (prefix <= max).then_some(Cidr { network, prefix })
        }
        None => {
            let network: IpAddr = spec.trim().parse().ok()?;
            let prefix = if network.is_ipv4() { 32 } else { 128 };
            Some(Cidr { network, prefix })
        }
    }
}

/// Whether `ip` falls within `cidr` (same family, prefix bits equal).
fn cidr_contains(cidr: &Cidr, ip: IpAddr) -> bool {
    match (cidr.network, ip) {
        (IpAddr::V4(net), IpAddr::V4(ip)) => {
            let mask = if cidr.prefix == 0 { 0 } else { u32::MAX << (32 - cidr.prefix) };
            (u32::from(net) & mask) == (u32::from(ip) & mask)
        }
        (IpAddr::V6(net), IpAddr::V6(ip)) => {
            let mask = if cidr.prefix == 0 { 0 } else { u128::MAX << (128 - cidr.prefix) };
            (u128::from(net) & mask) == (u128::from(ip) & mask)
        }
        _ => false, // different address families never match
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_direct_client_uses_the_peer_and_ignores_forwarded_headers() {
        // Nobody trusted → the request came straight to us; X-Forwarded-For is
        // attacker-controlled and must be ignored.
        let resolved = client_ip(Some(sa("1.2.3.4:5000")), Some("8.8.8.8"), &[]);
        assert_eq!(resolved, Some(ip("1.2.3.4")), "spoofed forwarded header ignored");
    }

    #[test]
    fn behind_a_trusted_proxy_uses_the_forwarded_client() {
        let trusted = [ip("10.0.0.1")];
        let resolved = client_ip(Some(sa("10.0.0.1:5000")), Some("1.2.3.4"), &trusted);
        assert_eq!(resolved, Some(ip("1.2.3.4")), "the real client from the proxy");
    }

    #[test]
    fn a_spoofed_left_hand_entry_is_defeated() {
        // XFF = "<spoofed>, <real client>, <proxy1>"; peer is proxy2 (trusted).
        // Walking right-to-left: proxy1 trusted (skip), then the real client — never
        // the spoofed 6.6.6.6 further left.
        let trusted = [ip("10.0.0.1"), ip("10.0.0.2")];
        let resolved = client_ip(Some(sa("10.0.0.2:5000")), Some("6.6.6.6, 1.2.3.4, 10.0.0.1"), &trusted);
        assert_eq!(resolved, Some(ip("1.2.3.4")), "the spoofed entry is shadowed by the real hops");
    }

    #[test]
    fn a_trusted_peer_with_no_forwarded_header_uses_the_peer() {
        let trusted = [ip("10.0.0.1")];
        assert_eq!(client_ip(Some(sa("10.0.0.1:5000")), None, &trusted), Some(ip("10.0.0.1")));
    }

    #[test]
    fn no_peer_address_is_none() {
        assert_eq!(client_ip(None, Some("1.2.3.4"), &[]), None);
    }

    #[test]
    fn the_visitor_cookie_round_trips() {
        let cookie = visitor_cookie("abc123");
        assert!(cookie.starts_with("kw_visitor=abc123;"));
        assert!(cookie.contains("HttpOnly"));
        assert_eq!(visitor_from_cookie("other=x; kw_visitor=abc123; more=y"), Some("abc123"));
        assert_eq!(visitor_from_cookie("nothing=here"), None);
    }

    #[test]
    fn bans_by_ip_subnet_and_user_agent() {
        let bans = BanList::new()
            .ip(ip("1.2.3.4"))
            .subnet("10.0.0.0/8")
            .subnet("192.168.1.0/24")
            .user_agent_exact("BadBot/1.0")
            .user_agent_containing("scraper");

        // Exact IP.
        assert!(bans.is_banned(Some(ip("1.2.3.4")), None));
        assert!(!bans.is_banned(Some(ip("1.2.3.5")), None));
        // Subnets.
        assert!(bans.is_banned(Some(ip("10.99.1.1")), None), "/8 covers 10.*");
        assert!(!bans.is_banned(Some(ip("11.0.0.1")), None));
        assert!(bans.is_banned(Some(ip("192.168.1.50")), None), "/24 covers .1.*");
        assert!(!bans.is_banned(Some(ip("192.168.2.50")), None), "/24 excludes .2.*");
        // User-Agent exact and contains (case-insensitive).
        assert!(bans.is_banned(None, Some("BadBot/1.0")));
        assert!(!bans.is_banned(None, Some("BadBot/2.0")));
        assert!(bans.is_banned(None, Some("Evil SCRAPER 3.0")), "contains is case-insensitive");
        // A clean request.
        assert!(!bans.is_banned(Some(ip("8.8.8.8")), Some("Mozilla/5.0")));
        // An empty list bans nobody, and a malformed subnet is ignored.
        assert!(!BanList::new().is_banned(Some(ip("1.2.3.4")), Some("BadBot/1.0")));
        assert!(!BanList::new().subnet("not-a-cidr").is_banned(Some(ip("1.2.3.4")), None));
    }

    #[test]
    fn ipv6_subnet_matching() {
        let bans = BanList::new().subnet("2001:db8::/32");
        assert!(bans.is_banned(Some(ip("2001:db8::1")), None));
        assert!(!bans.is_banned(Some(ip("2001:db9::1")), None));
        // A v4 address never matches a v6 rule.
        assert!(!bans.is_banned(Some(ip("1.2.3.4")), None));
    }
}
