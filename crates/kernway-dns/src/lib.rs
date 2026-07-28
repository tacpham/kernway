//! # kernway-dns
//!
//! A pure-async DNS **stub resolver** for the Kernway runtime — no
//! `getaddrinfo`, no blocking thread. It sends DNS-over-UDP queries through
//! [`kernway_udp::AsyncUdpSocket`] and parses the replies here.
//!
//! ## Scope (target "A" — a self-contained stub resolver)
//!
//! Handles the cases an outbound HTTP client actually meets: A/AAAA lookups
//! against the system's configured nameservers, with TCP fallback, caching, and
//! search-domain expansion added in later slices.
//!
//! **Deliberately not** full `getaddrinfo` parity: no NSS modules, no mDNS, no
//! macOS System-Configuration / systemd-resolved split-DNS. Those stay behind
//! the caller's `getaddrinfo` fallback. See the crate's design notes.
//!
//! ## Layers
//!
//! - [`message`] — DNS wire format (RFC 1035): encode a query, parse a response.
//!   Pure, no I/O, hardened against malformed packets (compression-pointer loops,
//!   truncation). This is the security-sensitive layer and is unit-testable
//!   without a server.
//! - [`system`] — `/etc/resolv.conf` and `/etc/hosts` parsing.
//! - [`resolver`] — the async [`Resolver`]: send a query over UDP, validate
//!   (random id + source port), parse the reply.
//! - [`cache`] — a per-shard, TTL-honouring cache (positive + negative).
#![deny(unsafe_op_in_unsafe_fn)]

pub mod cache;
pub mod message;
pub mod resolver;
pub mod system;

pub use resolver::Resolver;

/// Everything DNS resolution can fail with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsError {
    /// The packet ended in the middle of a field we were reading.
    Truncated,
    /// A name was malformed: a reserved length prefix, a forward/self pointer
    /// (a compression loop), or too many pointer jumps.
    MalformedName,
    /// A label exceeded 63 bytes, or a name exceeded 255 bytes.
    NameTooLong,
    /// The reply's transaction id or question did not match the query — a
    /// spoofed or stale datagram.
    Mismatch,
    /// The server answered `NXDOMAIN` — the name authoritatively does not exist.
    NameNotFound,
    /// The server returned a non-zero RCODE other than NXDOMAIN (SERVFAIL,
    /// REFUSED, …). Carries the raw RCODE.
    ServerFailure(u8),
    /// A well-formed reply carried no A/AAAA records for the name.
    NoAddresses,
    /// Underlying socket / timeout error.
    Io(String),
}

impl std::fmt::Display for DnsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DnsError::Truncated => write!(f, "DNS packet truncated"),
            DnsError::MalformedName => write!(f, "malformed DNS name (bad pointer or label)"),
            DnsError::NameTooLong => write!(f, "DNS name or label too long"),
            DnsError::Mismatch => write!(f, "DNS reply did not match the query"),
            DnsError::NameNotFound => write!(f, "name does not exist (NXDOMAIN)"),
            DnsError::ServerFailure(rcode) => write!(f, "DNS server error (RCODE {rcode})"),
            DnsError::NoAddresses => write!(f, "no A/AAAA records for the name"),
            DnsError::Io(e) => write!(f, "DNS I/O error: {e}"),
        }
    }
}

impl std::error::Error for DnsError {}
