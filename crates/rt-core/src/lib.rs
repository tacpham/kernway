//! # rt-core
//!
//! Kernway's async runtime: a thread-per-core Reactor + Executor.
//!
//! One shard = one OS thread = one [`Executor`] + one [`Reactor`]. Tasks are
//! never stolen or migrated between shards, so:
//!
//! - spawned futures need not be `Send` — `Rc` and `RefCell` are fine across an
//!   `await`;
//! - there is no work-stealing deque, no cross-thread task queue, and no atomic
//!   traffic on the polling hot path.
//!
//! Only *wakeups* cross threads (a `spawn_blocking` worker finishing, a timer),
//! and those go through an `Arc`-backed [`Waker`](std::task::Waker) plus a
//! `mio::Waker` that unparks the sleeping reactor. See [`task`] for why the
//! waker is not `Rc`-backed.
//!
//! `mio` supplies epoll/kqueue/IOCP; everything above it — Task, Waker,
//! Executor, Reactor, blocking pool — is written here.
//!
//! ## Example
//! ```
//! use rt_core::Executor;
//!
//! let ex = Executor::new().unwrap();
//! let answer = ex.block_on(async { 6 * 7 }).unwrap();
//! assert_eq!(answer, 42);
//! ```
//!
//! ## Unsafe
//! Confined to two places, each with a `SAFETY:` note per operation: the
//! `RawWakerVTable` in [`task`], and the `libc` affinity call in [`sys`].
#![deny(unsafe_op_in_unsafe_fn)]

pub mod blocking;
pub mod executor;
pub mod reactor;
pub mod shutdown;
pub mod sys;
pub mod task;
pub mod time;

pub use blocking::{spawn_blocking, Blocking};
pub use executor::{spawn, try_handle, try_with_reactor, with_reactor, Executor, Handle};
pub use reactor::{Direction, Reactor};
pub use shutdown::{until_shutdown, Shutdown};
pub use sys::{default_shard_count, on_interrupt, pin_current_thread_to_core};
pub use task::TaskId;
pub use time::{sleep, timeout, Elapsed};
