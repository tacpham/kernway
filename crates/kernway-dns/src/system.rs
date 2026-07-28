//! System resolver configuration — `/etc/resolv.conf` and `/etc/hosts`.
//!
//! The parsers are pure (take a `&str`) so they unit-test without touching the
//! filesystem. The `load_*` helpers read the real files and fall back to sane
//! defaults when a file is missing or unreadable.
//!
//! Note: this reads `/etc/resolv.conf` directly. On macOS (System Configuration)
//! and systemd-resolved split-DNS setups that file is not the whole story — those
//! cases are intentionally left to the caller's `getaddrinfo` fallback (target
//! "B", out of scope).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// Public resolvers used when `/etc/resolv.conf` names none — Google + Cloudflare.
const FALLBACK_SERVERS: [&str; 2] = ["8.8.8.8:53", "1.1.1.1:53"];

/// Parsed `/etc/resolv.conf`: the nameservers plus retry and search knobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvConf {
    /// Nameserver addresses, in file order (port 53 assumed).
    pub servers: Vec<SocketAddr>,
    /// Per-query timeout (`options timeout:N`, default 2 s, min 1 s).
    pub timeout: Duration,
    /// Query attempts across the server list (`options attempts:N`, default 2).
    pub attempts: usize,
    /// Search domains (`search` / `domain`), applied to names with fewer than
    /// `ndots` dots.
    pub search: Vec<String>,
    /// The dot threshold (`options ndots:N`, default 1): a name with at least
    /// this many dots is tried absolute first, otherwise search-suffixed first.
    pub ndots: u8,
}

impl Default for ResolvConf {
    fn default() -> Self {
        Self {
            servers: FALLBACK_SERVERS.iter().map(|s| s.parse().unwrap()).collect(),
            timeout: Duration::from_secs(2),
            attempts: 2,
            search: Vec::new(),
            ndots: 1,
        }
    }
}

/// Parse the contents of a `resolv.conf`. Unknown directives are ignored; if no
/// `nameserver` line is present the fallback public resolvers are used.
pub fn parse_resolv_conf(content: &str) -> ResolvConf {
    let mut servers = Vec::new();
    let mut timeout = Duration::from_secs(2);
    let mut attempts = 2usize;
    let mut search: Vec<String> = Vec::new();
    let mut ndots = 1u8;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let mut it = line.split_whitespace();
        match it.next() {
            Some("nameserver") => {
                if let Some(ip) = it.next().and_then(|s| s.parse::<IpAddr>().ok()) {
                    servers.push(SocketAddr::new(ip, 53));
                }
            }
            // `search` and `domain` are mutually exclusive; the last one in the
            // file wins, so a later directive replaces the list.
            Some("search") => {
                search = it.map(|s| s.to_owned()).collect();
            }
            Some("domain") => {
                search = it.next().map(|d| vec![d.to_owned()]).unwrap_or_default();
            }
            Some("options") => {
                for opt in it {
                    if let Some(v) = opt.strip_prefix("timeout:") {
                        if let Ok(n) = v.parse::<u64>() {
                            timeout = Duration::from_secs(n.max(1));
                        }
                    } else if let Some(v) = opt.strip_prefix("attempts:") {
                        if let Ok(n) = v.parse::<usize>() {
                            attempts = n.clamp(1, 10);
                        }
                    } else if let Some(v) = opt.strip_prefix("ndots:") {
                        if let Ok(n) = v.parse::<u8>() {
                            ndots = n.min(15);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let servers = if servers.is_empty() {
        ResolvConf::default().servers
    } else {
        servers
    };
    ResolvConf { servers, timeout, attempts, search, ndots }
}

/// Parse an `/etc/hosts` file into a hostname → addresses map (keys lowercased).
///
/// Inline `#` comments are honoured. A host may map to both an IPv4 and IPv6
/// address (e.g. `localhost`).
pub fn parse_hosts(content: &str) -> HashMap<String, Vec<IpAddr>> {
    let mut map: HashMap<String, Vec<IpAddr>> = HashMap::new();
    for raw in content.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(ip) = it.next().and_then(|s| s.parse::<IpAddr>().ok()) else {
            continue;
        };
        for host in it {
            map.entry(host.to_ascii_lowercase()).or_default().push(ip);
        }
    }
    map
}

/// Load `/etc/resolv.conf`, falling back to the public resolvers on any error.
pub fn load_resolv_conf() -> ResolvConf {
    std::fs::read_to_string("/etc/resolv.conf")
        .map(|c| parse_resolv_conf(&c))
        .unwrap_or_default()
}

/// Load `/etc/hosts`, returning an empty map on any error.
pub fn load_hosts() -> HashMap<String, Vec<IpAddr>> {
    std::fs::read_to_string("/etc/hosts")
        .map(|c| parse_hosts(&c))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn parses_nameservers_in_order() {
        let conf = parse_resolv_conf("nameserver 1.1.1.1\nnameserver 8.8.4.4\n");
        assert_eq!(
            conf.servers,
            vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)), 53),
            ]
        );
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let conf = parse_resolv_conf("# a comment\n; another\n\nnameserver 9.9.9.9\n");
        assert_eq!(conf.servers.len(), 1);
    }

    #[test]
    fn reads_timeout_and_attempts_options() {
        let conf = parse_resolv_conf("nameserver 8.8.8.8\noptions timeout:5 attempts:4\n");
        assert_eq!(conf.timeout, Duration::from_secs(5));
        assert_eq!(conf.attempts, 4);
    }

    #[test]
    fn no_nameserver_falls_back_to_public_resolvers() {
        let conf = parse_resolv_conf("options ndots:2\n");
        assert_eq!(conf.servers, ResolvConf::default().servers);
    }

    #[test]
    fn parses_search_domains_and_ndots() {
        let conf = parse_resolv_conf(
            "nameserver 8.8.8.8\nsearch corp.example lan.example\noptions ndots:2\n",
        );
        assert_eq!(conf.search, vec!["corp.example", "lan.example"]);
        assert_eq!(conf.ndots, 2);
    }

    #[test]
    fn domain_is_a_single_element_search_and_last_directive_wins() {
        let conf = parse_resolv_conf("search a.example b.example\ndomain only.example\n");
        assert_eq!(conf.search, vec!["only.example"]);
    }

    #[test]
    fn parses_hosts_including_localhost_v4_and_v6() {
        let hosts = parse_hosts("127.0.0.1 localhost\n::1 localhost ip6-localhost\n10.0.0.5 db  # inline\n");
        assert_eq!(
            hosts["localhost"],
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)]
        );
        assert_eq!(hosts["db"], vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))]);
        assert_eq!(hosts["ip6-localhost"], vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]);
    }
}
