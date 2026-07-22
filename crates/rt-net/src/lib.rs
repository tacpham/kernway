//! # rt-net
//!
//! Async TCP for Kernway, driven by [`rt_core`]'s per-shard reactor.
//!
//! ```no_run
//! use rt_core::Executor;
//! use rt_net::AsyncTcpListener;
//!
//! let ex = Executor::new().unwrap();
//! ex.block_on(async {
//!     let mut listener = AsyncTcpListener::bind("127.0.0.1:8080".parse().unwrap())?;
//!     loop {
//!         let (mut stream, _peer) = listener.accept().await?;
//!         rt_core::spawn(async move {
//!             let mut buf = [0u8; 1024];
//!             while let Ok(n) = stream.read(&mut buf).await {
//!                 if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
//!                     break;
//!                 }
//!             }
//!         });
//!     }
//!     # #[allow(unreachable_code)] Ok::<_, std::io::Error>(())
//! }).unwrap().unwrap();
//! ```
//!
//! For the multi-core setup use [`run_shards`], which binds one
//! `SO_REUSEPORT` listener per core and gives each its own executor — no shared
//! accept queue, no cross-thread task migration.
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(missing_docs)] // v0.2

pub mod listener;
pub mod shard;
pub mod stream;
pub mod sys;

pub use listener::AsyncTcpListener;
pub use shard::{bootstrap_shards, run_shards, ShardConfig};
pub use stream::AsyncTcpStream;
pub use sys::{balances_reuseport, supports_reuseport};
