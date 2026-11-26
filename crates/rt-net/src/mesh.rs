//! Core mesh — one [`mailbox`](fn@rt_core::mailbox) per shard, addressable by
//! index or by key.
//!
//! Some state has exactly one home shard: a game room, a document being
//! co-edited, a rate-limit bucket. Connections that touch it are accepted on
//! whatever shard the kernel picked, so their tasks must *send* work to the
//! owner instead of touching the state directly. A [`CoreMesh`] is that
//! address book: a `Send + Sync + Clone` table of one
//! [`MailboxSender`] per shard, plus a stable `key → shard` map so every shard
//! agrees on where a key lives.
//!
//! Wiring, in three steps (see [`run_shards_with_init`](crate::run_shards_with_init)
//! for a complete example):
//!
//! 1. Before starting the shards, build the mesh with
//!    [`CoreMesh::new`]`(`[`shard_thread_count`](crate::shard_thread_count)`(&config), capacity)`.
//!    It returns the mesh and one [`MailboxReceiver`] per shard.
//! 2. In the per-shard `init` hook, shard `i` takes receiver `i` and spawns
//!    the task that consumes it.
//! 3. Anywhere — any shard, any thread — clone the mesh and
//!    [`send_to`](CoreMesh::send_to)`(`[`core_for`](CoreMesh::core_for)`(key), msg)`.

use std::fmt;
use std::sync::Arc;

use rt_core::{mailbox, MailboxReceiver, MailboxSender, TrySendError};

/// One mailbox sender per shard. Cloning is a reference-count bump.
pub struct CoreMesh<T> {
    senders: Arc<[MailboxSender<T>]>,
}

impl<T: Send> CoreMesh<T> {
    /// Build a mesh of `shards` mailboxes, each holding up to `capacity`
    /// messages. Receiver `i` belongs to shard `i`.
    ///
    /// # Panics
    /// If `shards` or `capacity` is zero.
    pub fn new(shards: usize, capacity: usize) -> (Self, Vec<MailboxReceiver<T>>) {
        assert!(shards > 0, "a core mesh needs at least one shard");
        let (senders, receivers): (Vec<_>, Vec<_>) = (0..shards).map(|_| mailbox(capacity)).unzip();
        (
            Self {
                senders: senders.into(),
            },
            receivers,
        )
    }
}

impl<T> CoreMesh<T> {
    /// Send `msg` to shard `core` without blocking.
    ///
    /// # Errors
    /// [`TrySendError::Full`] when that shard's mailbox is at capacity
    /// (backpressure — the shard is behind), [`TrySendError::Closed`] when its
    /// receiver has been dropped. Both hand the message back.
    ///
    /// # Panics
    /// If `core >= self.len()` — an index outside the mesh is a routing bug,
    /// not a runtime condition.
    pub fn send_to(&self, core: usize, msg: T) -> Result<(), TrySendError<T>> {
        self.sender(core).try_send(msg)
    }

    /// Send `msg` to the shard that owns `key` — shorthand for
    /// `send_to(core_for(key), msg)`.
    ///
    /// # Errors
    /// As [`send_to`](Self::send_to).
    pub fn send_by_key(&self, key: u64, msg: T) -> Result<(), TrySendError<T>> {
        self.send_to(self.core_for(key), msg)
    }

    /// The shard that owns `key`.
    ///
    /// Deterministic for a given `(key, len())`: the same on every shard, every
    /// thread, every process and every build — it is a fixed integer mix, not
    /// [`std::hash`]'s randomly seeded hasher, so a restarted node routes keys
    /// exactly as before. Changing the shard count moves keys, as any modulo
    /// scheme does.
    ///
    /// Keys are scrambled before reduction, so sequential ids (room 1, 2, 3…)
    /// spread evenly rather than striping. For a string id, hash it to a
    /// `u64` with a stable hash first.
    pub fn core_for(&self, key: u64) -> usize {
        core_for_key(key, self.senders.len())
    }

    /// The sender for shard `core`, for callers that want to keep it.
    ///
    /// # Panics
    /// If `core >= self.len()`.
    pub fn sender(&self, core: usize) -> &MailboxSender<T> {
        let len = self.senders.len();
        self.senders
            .get(core)
            .unwrap_or_else(|| panic!("core {core} is outside a mesh of {len} shard(s)"))
    }

    /// The calling shard's own index in this mesh, or `None` off-shard.
    ///
    /// Lets a task short-circuit a message to itself: when
    /// `mesh.local() == Some(mesh.core_for(key))`, the state is already here.
    pub fn local(&self) -> Option<usize> {
        crate::shard_index().filter(|&index| index < self.senders.len())
    }

    /// Number of shards in the mesh.
    pub fn len(&self) -> usize {
        self.senders.len()
    }

    /// Always `false` — a mesh has at least one shard. Present for API
    /// symmetry with [`len`](Self::len).
    pub fn is_empty(&self) -> bool {
        self.senders.is_empty()
    }
}

impl<T> Clone for CoreMesh<T> {
    fn clone(&self) -> Self {
        Self {
            senders: Arc::clone(&self.senders),
        }
    }
}

impl<T> fmt::Debug for CoreMesh<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreMesh")
            .field("shards", &self.senders.len())
            .finish_non_exhaustive()
    }
}

/// Map `key` onto `0..cores`.
///
/// SplitMix64's finalizer scrambles the key (every input bit affects every
/// output bit), then Lemire's multiply-shift maps the scrambled value onto the
/// range using its high bits — no division, and no reliance on the low bits
/// alone the way `% cores` would. Both are fixed arithmetic, so the result is
/// stable forever.
fn core_for_key(key: u64, cores: usize) -> usize {
    let mut z = key.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    ((u128::from(z) * cores as u128) >> 64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_core::Executor;
    use std::time::Duration;

    #[test]
    fn the_mesh_is_shareable_across_threads() {
        fn assert_send_sync_clone<X: Send + Sync + Clone>() {}
        assert_send_sync_clone::<CoreMesh<String>>();
    }

    #[test]
    fn send_to_reaches_exactly_that_shards_receiver() {
        let (mesh, mut receivers) = CoreMesh::<usize>::new(4, 8);
        assert_eq!(mesh.len(), 4);
        assert_eq!(receivers.len(), 4);
        for core in 0..4 {
            mesh.send_to(core, core * 10).unwrap();
        }
        for (core, rx) in receivers.iter_mut().enumerate() {
            assert_eq!(rx.try_recv(), Ok(core * 10));
            assert!(rx.is_empty(), "shard {core} got someone else's message");
        }
    }

    #[test]
    fn send_by_key_routes_to_core_for() {
        let (mesh, mut receivers) = CoreMesh::<u64>::new(3, 64);
        for key in 0..30 {
            mesh.send_by_key(key, key).unwrap();
        }
        for (core, rx) in receivers.iter_mut().enumerate() {
            while let Ok(key) = rx.try_recv() {
                assert_eq!(
                    mesh.core_for(key),
                    core,
                    "key {key} landed on the wrong shard"
                );
            }
        }
    }

    #[test]
    fn backpressure_is_per_shard() {
        let (mesh, _receivers) = CoreMesh::<u8>::new(2, 1);
        mesh.send_to(0, 1).unwrap();
        assert!(mesh.send_to(0, 2).unwrap_err().is_full());
        mesh.send_to(1, 3)
            .expect("a full shard must not block its neighbours");
    }

    #[test]
    fn a_dropped_receiver_closes_only_its_shard() {
        let (mesh, mut receivers) = CoreMesh::<u8>::new(2, 4);
        drop(receivers.remove(1));
        assert!(mesh.send_to(1, 0).unwrap_err().is_closed());
        mesh.send_to(0, 0).unwrap();
    }

    #[test]
    #[should_panic(expected = "outside a mesh")]
    fn an_out_of_range_core_panics() {
        let (mesh, _receivers) = CoreMesh::<u8>::new(2, 4);
        let _ = mesh.send_to(2, 0);
    }

    #[test]
    fn core_for_is_stable() {
        // Pinned values: these must never change, or a rolling restart would
        // re-home every room. If this fails, the hash changed — that is a
        // breaking change, not a test to update casually.
        let (mesh, _rx) = CoreMesh::<u8>::new(8, 1);
        let pinned: Vec<usize> = (0..8).map(|k| mesh.core_for(k)).collect();
        assert_eq!(pinned, PINNED_8);
        // And deterministic within a run, across clones.
        let clone = mesh.clone();
        for key in [0, 1, 42, u64::MAX, 0xDEAD_BEEF] {
            assert_eq!(mesh.core_for(key), clone.core_for(key));
        }
    }

    /// `core_for(0..8)` on an 8-shard mesh. See `core_for_is_stable`.
    const PINNED_8: [usize; 8] = [7, 4, 4, 0, 3, 3, 5, 3];

    #[test]
    fn core_for_spreads_sequential_keys_evenly() {
        for cores in [1, 2, 3, 4, 7, 8, 16, 64] {
            let (mesh, _rx) = CoreMesh::<u8>::new(cores, 1);
            let mut counts = vec![0usize; cores];
            const KEYS: usize = 100_000;
            for key in 0..KEYS as u64 {
                let core = mesh.core_for(key);
                assert!(core < cores);
                counts[core] += 1;
            }
            let expected = KEYS / cores;
            for (core, &n) in counts.iter().enumerate() {
                // ±10% of a fair share is far looser than a good mix needs and
                // far tighter than a broken one (striping, a stuck bit) achieves.
                assert!(
                    n.abs_diff(expected) * 10 <= expected,
                    "{cores} cores: shard {core} got {n} of {KEYS} keys (fair share {expected})"
                );
            }
        }
    }

    #[test]
    fn a_shard_thread_receives_from_another_thread() {
        // End to end: a receiver parked in its own executor on one thread, the
        // mesh used from another, with a deadline so a lost wakeup fails.
        let (mesh, mut receivers) = CoreMesh::<&'static str>::new(2, 8);
        let rx1 = receivers.pop().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut rx1 = rx1;
            let ex = Executor::new().unwrap();
            let got = ex.block_on(rx1.recv()).unwrap();
            let _ = done_tx.send(got);
        });
        std::thread::sleep(Duration::from_millis(20));
        mesh.send_to(1, "to shard 1").unwrap();
        let got = done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the shard never woke up");
        assert_eq!(got, Some("to shard 1"));
    }

    #[test]
    fn local_is_none_off_shard() {
        let (mesh, _rx) = CoreMesh::<u8>::new(2, 1);
        assert_eq!(mesh.local(), None);
    }
}
