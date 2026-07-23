//! Non-Unix platforms — currently Windows.
//!
//! Windows has no `SO_REUSEPORT`. The plan in
//! `docs/design/modules/rt-net.md` is a single shared listener whose accepts
//! are dispatched by IOCP; until that is written, binding falls back to one
//! ordinary listener and [`bootstrap_shards`](crate::bootstrap_shards) drops to
//! a single shard rather than pretending to scale.

use std::io;
use std::net::{SocketAddr, TcpListener};

pub(super) const SUPPORTS_REUSEPORT: bool = false;
pub(super) const BALANCES_REUSEPORT: bool = false;

pub(super) fn bind_listener(
    addr: SocketAddr,
    _backlog: i32,
    _reuseport: bool,
) -> io::Result<TcpListener> {
    // `backlog` is not configurable through the std API; it uses the system
    // default, which is adequate until the IOCP path lands.
    TcpListener::bind(addr)
}
