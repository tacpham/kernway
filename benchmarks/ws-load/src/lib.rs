//! Shared pieces of the `ws-load` harness: the latency histogram, the client
//! side of RFC 6455 framing, and process plumbing (fd limit, RSS, CPU time).
//!
//! The two binaries are `ws-load-server` (a `KernwayApp` with `/echo`,
//! `/room/{id}` and `/stats`) and `ws-load` (the load generator). See the
//! crate's README for how to run them.

pub mod hist;
pub mod sys;
pub mod wire;
