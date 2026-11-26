//! `kernway-ws` — WebSocket server (RFC 6455) for Kernway.
//!
//! A route answers the upgrade with [`upgrade`]; kernway-server writes the `101`
//! and hands the socket to the [`WebSocket`] session, which then runs on the core
//! that accepted the connection. See [`server`] for the performance model
//! (per-shard heartbeat, cursor buffer, word-wide unmasking, vectored writes,
//! shared broadcast frames, bounded outbox).
#![forbid(unsafe_code)]

pub mod frame;
pub mod handshake;
mod heartbeat;
pub mod server;

pub use frame::close_code;
pub use server::{
    upgrade, Message, PreparedMessage, SendError, WebSocket, WsConfig, WsReader, WsSender,
};
