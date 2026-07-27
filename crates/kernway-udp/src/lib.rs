//! # kernway-udp
//!
//! Async UDP socket for the Kernway runtime (`rt-core` / `rt-net`).
//!
//! ## Status: SKELETON — not yet implemented
//!
//! This crate is a placeholder for the async UDP layer that will power
//! [`kernway-dns`] (pure-async DNS resolver). Once complete, `kernway-http-client`
//! will replace its current `spawn_blocking(getaddrinfo)` DNS call with a proper
//! non-blocking resolver.
//!
//! ## Planned API
//!
//! ```rust,ignore
//! let sock = AsyncUdpSocket::bind("0.0.0.0:0").await?;
//! sock.send_to(buf, "8.8.8.8:53").await?;
//! let (n, from) = sock.recv_from(&mut buf).await?;
//! ```
//!
//! ## Roadmap
//!
//! 1. `AsyncUdpSocket` backed by the `rt-core` reactor (epoll/kqueue `EPOLLIN`)
//! 2. `send_to` / `recv_from` / `connect` / `send` / `recv`
//! 3. Used by `kernway-dns` for DNS-over-UDP (RFC 1035) with TCP fallback
//!
//! See also: `kernway-tcp` for higher-level TCP utilities.
