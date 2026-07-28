//! The async stub resolver: send a query over UDP, validate and parse the reply.
//!
//! ## Anti-spoofing
//!
//! Two defences a stub resolver must have, both here:
//! - a **random transaction id** from `getrandom` (a CSPRNG), and
//! - a **random source port** — each query binds a fresh `:0` socket, so the OS
//!   picks a new ephemeral port every time.
//!
//! On top of that the socket is `connect`ed to the nameserver, so the kernel
//! drops any datagram not from that address, and the parsed id is checked
//! against the one we sent. An off-path attacker must guess both a 16-bit id and
//! a 16-bit port within the timeout.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use kernway_udp::AsyncUdpSocket;
use rt_net::AsyncTcpStream;

use crate::message::{
    encode_query, encode_query_edns, parse_response, ResolvedAddr, EDNS_UDP_SIZE, TYPE_A, TYPE_AAAA,
};
use crate::system::{load_resolv_conf, ResolvConf};
use crate::DnsError;

/// UDP receive buffer — sized to the EDNS0 payload we advertise, so answers up
/// to that size arrive over UDP; anything larger sets the TC bit and we retry
/// over TCP.
const UDP_BUF: usize = EDNS_UDP_SIZE as usize;

/// RCODE for an authoritative "name does not exist".
const RCODE_NXDOMAIN: u8 = 3;

/// An async DNS stub resolver over UDP.
#[derive(Debug, Clone)]
pub struct Resolver {
    servers: Vec<SocketAddr>,
    timeout: Duration,
    attempts: usize,
    search: Vec<String>,
    ndots: u8,
}

impl Resolver {
    /// Build a resolver from an explicit nameserver list (used by tests and
    /// callers that configure DNS directly). Defaults: 2 s timeout, 2 attempts,
    /// no search domains.
    pub fn new(servers: Vec<SocketAddr>) -> Self {
        Self {
            servers,
            timeout: Duration::from_secs(2),
            attempts: 2,
            search: Vec::new(),
            ndots: 1,
        }
    }

    /// Build a resolver from `/etc/resolv.conf` (falling back to public
    /// resolvers if it names none).
    pub fn from_system() -> Self {
        Self::from_conf(load_resolv_conf())
    }

    /// Build a resolver from an already-parsed [`ResolvConf`].
    pub fn from_conf(conf: ResolvConf) -> Self {
        Self {
            servers: conf.servers,
            timeout: conf.timeout,
            attempts: conf.attempts,
            search: conf.search,
            ndots: conf.ndots,
        }
    }

    /// Override the per-query timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the number of attempts across the server list.
    #[must_use]
    pub fn with_attempts(mut self, attempts: usize) -> Self {
        self.attempts = attempts.max(1);
        self
    }

    /// Set the search domains and the `ndots` threshold.
    #[must_use]
    pub fn with_search(mut self, search: Vec<String>, ndots: u8) -> Self {
        self.search = search;
        self.ndots = ndots;
        self
    }

    /// Resolve `host` to its IPv4 (`A`) addresses.
    pub async fn lookup_a(&self, host: &str) -> Result<Vec<IpAddr>, DnsError> {
        self.lookup_family(host, TYPE_A).await
    }

    /// Resolve `host` to its IPv6 (`AAAA`) addresses.
    pub async fn lookup_aaaa(&self, host: &str) -> Result<Vec<IpAddr>, DnsError> {
        self.lookup_family(host, TYPE_AAAA).await
    }

    /// Resolve `host` to any usable address, preferring IPv4: return the `A`
    /// records if present, else fall back to `AAAA`. An authoritative
    /// `NXDOMAIN` is returned as-is (it applies to every record type).
    pub async fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, DnsError> {
        match self.lookup_a(host).await {
            Ok(v) if !v.is_empty() => Ok(v),
            Ok(_) | Err(DnsError::NoAddresses) => self.lookup_aaaa(host).await,
            Err(DnsError::NameNotFound) => Err(DnsError::NameNotFound),
            Err(_) => self.lookup_aaaa(host).await,
        }
    }

    /// Expand `host` into the ordered list of names to try, per the `search` /
    /// `ndots` rules. A trailing-dot (fully-qualified) name is never expanded.
    pub fn candidates(&self, host: &str) -> Vec<String> {
        if let Some(absolute) = host.strip_suffix('.') {
            return vec![absolute.to_owned()];
        }
        if self.search.is_empty() {
            return vec![host.to_owned()];
        }
        let suffixed = self.search.iter().map(|d| format!("{host}.{d}"));
        if host.matches('.').count() >= self.ndots as usize {
            // Enough dots → try the name as given first, then search domains.
            std::iter::once(host.to_owned()).chain(suffixed).collect()
        } else {
            // Too few dots → search domains first, bare name last.
            suffixed.chain(std::iter::once(host.to_owned())).collect()
        }
    }

    /// Resolve one record type, trying each search candidate until one yields
    /// addresses.
    async fn lookup_family(&self, host: &str, qtype: u16) -> Result<Vec<IpAddr>, DnsError> {
        let mut last = DnsError::NoAddresses;
        for candidate in self.candidates(host) {
            match self.resolve_cached(&candidate, qtype).await {
                Ok(ips) if !ips.is_empty() => return Ok(ips),
                Ok(_) => last = DnsError::NoAddresses,
                Err(DnsError::NameNotFound) => last = DnsError::NameNotFound,
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Resolve a single concrete name of one record type, consulting and
    /// populating the per-shard cache.
    async fn resolve_cached(&self, name: &str, qtype: u16) -> Result<Vec<IpAddr>, DnsError> {
        let key = cache_key(qtype, name);
        match crate::cache::get(&key) {
            Some(crate::cache::Cached::Hit(ips)) => return Ok(ips),
            Some(crate::cache::Cached::Negative) => return Err(DnsError::NameNotFound),
            None => {}
        }
        if self.servers.is_empty() {
            return Err(DnsError::Io("no nameservers configured".into()));
        }
        let mut last = DnsError::NoAddresses;
        for _ in 0..self.attempts {
            for &server in &self.servers {
                match self.query_one(server, name, qtype).await {
                    Ok(addrs) if !addrs.is_empty() => {
                        // Cache under the shortest TTL among the records; a TTL of
                        // 0 means "do not cache".
                        let ttl = addrs.iter().map(|a| a.ttl).min().unwrap_or(0);
                        let ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip).collect();
                        if ttl > 0 {
                            crate::cache::put_positive(
                                &key,
                                ips.clone(),
                                Duration::from_secs(ttl as u64),
                            );
                        }
                        return Ok(ips);
                    }
                    Ok(_) => last = DnsError::NoAddresses,
                    Err(DnsError::NameNotFound) => {
                        crate::cache::put_negative(&key, crate::cache::NEGATIVE_TTL);
                        return Err(DnsError::NameNotFound);
                    }
                    Err(e) => last = e,
                }
            }
        }
        Err(last)
    }

    /// One query→reply exchange with a single nameserver.
    async fn query_one(
        &self,
        server: SocketAddr,
        host: &str,
        qtype: u16,
    ) -> Result<Vec<ResolvedAddr>, DnsError> {
        let id = random_id();
        let query = encode_query_edns(id, host, qtype, EDNS_UDP_SIZE)?;

        // Fresh socket → fresh random source port. Bind family-matches the server.
        let bind: SocketAddr = if server.is_ipv6() {
            "[::]:0".parse().unwrap()
        } else {
            "0.0.0.0:0".parse().unwrap()
        };
        let sock = AsyncUdpSocket::bind(bind).map_err(|e| DnsError::Io(e.to_string()))?;
        // connect() filters incoming datagrams to this peer (source validation).
        sock.connect(server).map_err(|e| DnsError::Io(e.to_string()))?;
        sock.send(&query).await.map_err(|e| DnsError::Io(e.to_string()))?;

        let mut buf = [0u8; UDP_BUF];
        let n = match rt_core::timeout(self.timeout, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(DnsError::Io(e.to_string())),
            Err(_elapsed) => return Err(DnsError::Io(format!("timeout after {:?}", self.timeout))),
        };

        let resp = parse_response(&buf[..n])?;
        if resp.id != id {
            // A datagram from the right host but the wrong id — stale or spoofed.
            return Err(DnsError::Mismatch);
        }
        match resp.rcode {
            0 => {}
            RCODE_NXDOMAIN => return Err(DnsError::NameNotFound),
            other => return Err(DnsError::ServerFailure(other)),
        }
        // TC (truncated): the answer didn't fit even the EDNS0 UDP size — retry
        // the whole query over TCP, where a 2-byte length prefix frames answers
        // up to 64 KiB.
        if resp.truncated {
            return self.query_tcp(server, host, qtype).await;
        }
        Ok(resp.addresses)
    }

    /// The same query over TCP (RFC 1035 §4.2.2): a 2-byte big-endian length
    /// prefix in front of the message, both ways. Used only as a TC-bit fallback.
    async fn query_tcp(
        &self,
        server: SocketAddr,
        host: &str,
        qtype: u16,
    ) -> Result<Vec<ResolvedAddr>, DnsError> {
        let id = random_id();
        let query = encode_query(id, host, qtype)?;
        let mut framed = Vec::with_capacity(query.len() + 2);
        framed.extend_from_slice(&(query.len() as u16).to_be_bytes());
        framed.extend_from_slice(&query);

        let exchange = async {
            let mut stream = AsyncTcpStream::connect(server)
                .await
                .map_err(|e| DnsError::Io(e.to_string()))?;
            stream
                .write_all(&framed)
                .await
                .map_err(|e| DnsError::Io(e.to_string()))?;
            let mut len_buf = [0u8; 2];
            read_exact(&mut stream, &mut len_buf).await?;
            let len = u16::from_be_bytes(len_buf) as usize;
            let mut resp_buf = vec![0u8; len];
            read_exact(&mut stream, &mut resp_buf).await?;
            Ok::<Vec<u8>, DnsError>(resp_buf)
        };

        let resp_buf = match rt_core::timeout(self.timeout, exchange).await {
            Ok(r) => r?,
            Err(_elapsed) => {
                return Err(DnsError::Io(format!("TCP DNS timeout after {:?}", self.timeout)))
            }
        };

        let resp = parse_response(&resp_buf)?;
        if resp.id != id {
            return Err(DnsError::Mismatch);
        }
        match resp.rcode {
            0 => {}
            RCODE_NXDOMAIN => return Err(DnsError::NameNotFound),
            other => return Err(DnsError::ServerFailure(other)),
        }
        Ok(resp.addresses)
    }
}

/// Read exactly `buf.len()` bytes, or fail. A clean EOF before then is a
/// truncated TCP message.
async fn read_exact(stream: &mut AsyncTcpStream, buf: &mut [u8]) -> Result<(), DnsError> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = stream
            .read(&mut buf[filled..])
            .await
            .map_err(|e| DnsError::Io(e.to_string()))?;
        if n == 0 {
            return Err(DnsError::Truncated);
        }
        filled += n;
    }
    Ok(())
}

/// Cache key namespacing a name by record type, so A and AAAA don't collide.
fn cache_key(qtype: u16, name: &str) -> String {
    format!("{qtype}:{name}")
}

/// A random 16-bit transaction id from the system CSPRNG.
fn random_id() -> u16 {
    let mut b = [0u8; 2];
    // getrandom only fails if the OS RNG is unavailable — treat that as fatal
    // rather than falling back to a predictable id (which would be a real
    // spoofing weakness).
    getrandom::getrandom(&mut b).expect("system RNG unavailable");
    u16::from_be_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_are_not_all_equal() {
        // Not a statistical test — just a smoke check that the CSPRNG varies.
        let a = random_id();
        let differs = (0..16).any(|_| random_id() != a);
        assert!(differs, "transaction ids must not be constant");
    }

    #[test]
    fn empty_nameservers_is_an_error_without_touching_the_network() {
        let ex = rt_core::Executor::new().unwrap();
        let r = Resolver::new(vec![]);
        let out = ex.block_on(async move { r.lookup_a("example.com").await }).unwrap();
        assert!(matches!(out, Err(DnsError::Io(_))));
    }

    fn with_search(search: &[&str], ndots: u8) -> Resolver {
        Resolver::new(vec!["127.0.0.1:53".parse().unwrap()])
            .with_search(search.iter().map(|s| s.to_string()).collect(), ndots)
    }

    #[test]
    fn no_search_domains_yields_just_the_bare_name() {
        let r = Resolver::new(vec!["127.0.0.1:53".parse().unwrap()]);
        assert_eq!(r.candidates("web"), vec!["web"]);
    }

    #[test]
    fn a_fully_qualified_name_is_never_expanded() {
        let r = with_search(&["corp.example"], 1);
        assert_eq!(r.candidates("web.example.com."), vec!["web.example.com"]);
    }

    #[test]
    fn few_dots_tries_search_domains_first() {
        // "web" has 0 dots < ndots(1) → suffixes first, bare name last.
        let r = with_search(&["corp.example", "lan.example"], 1);
        assert_eq!(
            r.candidates("web"),
            vec!["web.corp.example", "web.lan.example", "web"]
        );
    }

    #[test]
    fn enough_dots_tries_the_absolute_name_first() {
        // "a.b" has 1 dot >= ndots(1) → bare name first, then suffixes.
        let r = with_search(&["corp.example"], 1);
        assert_eq!(r.candidates("a.b"), vec!["a.b", "a.b.corp.example"]);
    }
}
