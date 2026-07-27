//! # kernway-tcp
//!
//! Higher-level async TCP utilities on top of `rt-net`.
//!
//! ## Status: SKELETON — not yet implemented
//!
//! Planned features:
//!
//! - **Connection pooling**: generic `TcpPool<K>` (the ad-hoc pool in
//!   `kernway-http-client` will migrate here)
//! - **Framing**: length-prefixed and newline-delimited readers
//! - **Keep-alive probes**: TCP-level heartbeats with configurable interval
//!
//! ## Planned API
//!
//! ```rust,ignore
//! let pool = TcpPool::new(max_idle_per_host: 8);
//! let conn = pool.get_or_connect("api.example.com:443").await?;
//! conn.write_all(b"GET / HTTP/1.1\r\n\r\n").await?;
//! ```
//!
//! See also: `kernway-udp` for the async UDP socket layer.
