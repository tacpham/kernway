//! Task, the shared wake queue, and the hand-rolled `RawWaker`.
//!
//! # Why the waker is not backed by `Rc<Task>`
//!
//! `docs/design/modules/rt-core.md` sketches `make_waker(task: Rc<Task>)`.
//! That sketch is unsound: [`Waker`] is `Send + Sync`, so a future may hand its
//! waker to a timer thread, a `spawn_blocking` worker, or any other thread and
//! have `wake()` called there. An `Rc` refcount touched from two threads is a
//! data race, i.e. UB — and this is not a corner case, it is how a runtime
//! normally gets woken.
//!
//! So the split is:
//!
//! - `Task` — holds the future, lives in an `Rc` inside the owning shard's
//!   slab, and **never leaves its thread**. The future therefore does not need
//!   to be `Send`, which is the whole point of thread-per-core.
//! - `WakeHandle` — the waker payload: an `Arc` carrying only a [`TaskId`]
//!   plus a handle to the shard's `Shared` queue. Sound to move and wake
//!   across threads; waking just enqueues the id and unparks the reactor.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// Slot index of a task within its shard.
pub type TaskId = usize;

/// Pseudo-id for the future driven by [`Executor::block_on`](crate::Executor::block_on).
///
/// It has no slab slot — waking it only needs to break the reactor out of its
/// park so the loop re-polls the blocked-on future.
pub(crate) const MAIN_TASK: TaskId = usize::MAX;

/// Cross-thread half of a shard: the wake queue plus the means to interrupt a
/// reactor that is parked inside `poll()`.
pub(crate) struct Shared {
    /// Ids woken since the last drain. `Mutex` rather than a lock-free queue
    /// because the same-thread case is uncontended and this is not yet the
    /// measured bottleneck — revisit with the v0.2 echo benchmark.
    ready: Mutex<VecDeque<TaskId>>,
    /// Wakes a reactor blocked in `mio::Poll::poll`.
    unparker: mio::Waker,
    /// `true` while the reactor is parked — lets same-thread wakes skip the
    /// `unparker.wake()` syscall entirely.
    parked: AtomicBool,
    /// Set when [`MAIN_TASK`] is woken. Without it a wake landing between the
    /// executor's "anything ready?" check and its park would be lost, and
    /// `block_on` would sleep forever holding a future that is actually ready.
    main_woken: AtomicBool,
}

impl Shared {
    pub(crate) fn new(unparker: mio::Waker) -> Self {
        Self {
            ready: Mutex::new(VecDeque::new()),
            unparker,
            parked: AtomicBool::new(false),
            main_woken: AtomicBool::new(false),
        }
    }

    /// Queue `id` to be polled, and unpark the reactor if it is sleeping.
    pub(crate) fn schedule(&self, id: TaskId) {
        if id == MAIN_TASK {
            self.main_woken.store(true, Ordering::Release);
        } else {
            self.lock_ready().push_back(id);
        }
        // Only the parked→awake transition sends the wakeup, so a burst of
        // wakes costs at most one syscall.
        if self.parked.swap(false, Ordering::AcqRel) {
            let _ = self.unparker.wake();
        }
    }

    /// Move every queued id into `out`, leaving the queue empty.
    pub(crate) fn drain_into(&self, out: &mut VecDeque<TaskId>) {
        let mut ready = self.lock_ready();
        out.append(&mut ready);
    }

    pub(crate) fn has_ready(&self) -> bool {
        !self.lock_ready().is_empty()
    }

    pub(crate) fn set_parked(&self, parked: bool) {
        self.parked.store(parked, Ordering::Release);
    }

    /// Consume the "`block_on` future was woken" flag.
    pub(crate) fn take_main_woken(&self) -> bool {
        self.main_woken.swap(false, Ordering::AcqRel)
    }

    /// A panic in a task must not poison the whole shard's queue: recover the
    /// guard instead of propagating.
    fn lock_ready(&self) -> std::sync::MutexGuard<'_, VecDeque<TaskId>> {
        self.ready.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A spawned future plus its slot id.
///
/// The future is `!Send`: it is polled only on the thread that spawned it.
pub(crate) struct Task {
    /// `None` once the future has completed — a task woken after completion is
    /// simply dropped rather than polled again.
    future: RefCell<Option<Pin<Box<dyn Future<Output = ()>>>>>,
    waker: Waker,
}

impl Task {
    pub(crate) fn new(
        future: Pin<Box<dyn Future<Output = ()>>>,
        id: TaskId,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            future: RefCell::new(Some(future)),
            waker: waker_for(id, shared),
        }
    }

    /// Poll the future once. Returns `true` when it has completed.
    ///
    /// Re-entrancy is impossible: the executor polls one task at a time on this
    /// thread, and a self-wake during the poll only re-queues the id.
    pub(crate) fn poll(&self) -> bool {
        let mut slot = self.future.borrow_mut();
        let Some(future) = slot.as_mut() else {
            return true; // already finished
        };
        let mut cx = Context::from_waker(&self.waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(()) => {
                *slot = None;
                true
            }
            Poll::Pending => false,
        }
    }
}

// ---------------------------------------------------------------------------
// RawWaker — payload is `Arc<WakeHandle>`.
// ---------------------------------------------------------------------------

/// What a [`Waker`] actually points at: an id and the shard to wake it on.
struct WakeHandle {
    id: TaskId,
    shared: Arc<Shared>,
}

/// Build a `Waker` that schedules `id` on `shared`.
pub(crate) fn waker_for(id: TaskId, shared: Arc<Shared>) -> Waker {
    let handle = Arc::new(WakeHandle { id, shared });
    let ptr = Arc::into_raw(handle) as *const ();
    // SAFETY: `ptr` came from `Arc::into_raw` on `Arc<WakeHandle>`, and every
    // function in VTABLE reinterprets it as exactly that type. Ownership of the
    // strong count moves into the returned `Waker`, which the vtable's `drop`
    // gives back. `Arc<WakeHandle>` is `Send + Sync` (`TaskId` is `usize` and
    // `Shared` is `Sync`), satisfying `Waker`'s own `Send + Sync` contract.
    unsafe { Waker::from_raw(RawWaker::new(ptr, &VTABLE)) }
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);

/// # Safety
/// `ptr` must be a pointer obtained from `Arc::<WakeHandle>::into_raw` whose
/// strong reference is still live.
unsafe fn clone_fn(ptr: *const ()) -> RawWaker {
    // SAFETY: caller guarantees `ptr` is a live `Arc<WakeHandle>` pointer;
    // `increment_strong_count` accounts for the new `RawWaker` sharing it.
    unsafe { Arc::increment_strong_count(ptr as *const WakeHandle) };
    RawWaker::new(ptr, &VTABLE)
}

/// # Safety
/// As [`clone_fn`]; consumes the strong reference.
unsafe fn wake_fn(ptr: *const ()) {
    // SAFETY: caller guarantees `ptr` came from `Arc::into_raw`; taking it back
    // with `from_raw` consumes exactly the one reference this waker owned.
    let handle = unsafe { Arc::from_raw(ptr as *const WakeHandle) };
    handle.shared.schedule(handle.id);
}

/// # Safety
/// As [`clone_fn`]; leaves the strong reference intact.
unsafe fn wake_by_ref_fn(ptr: *const ()) {
    // SAFETY: as above, but `ManuallyDrop` keeps the count unchanged because
    // this waker is only borrowed, not consumed.
    let handle = unsafe { std::mem::ManuallyDrop::new(Arc::from_raw(ptr as *const WakeHandle)) };
    handle.shared.schedule(handle.id);
}

/// # Safety
/// As [`clone_fn`]; consumes the strong reference.
unsafe fn drop_fn(ptr: *const ()) {
    // SAFETY: caller guarantees `ptr` came from `Arc::into_raw` and that this
    // waker still owns the reference being released here.
    drop(unsafe { Arc::from_raw(ptr as *const WakeHandle) });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Arc<Shared> {
        let poll = mio::Poll::new().unwrap();
        let unparker = mio::Waker::new(poll.registry(), mio::Token(0)).unwrap();
        // The `Poll` is dropped here; `mio::Waker` keeps its own handle alive,
        // which is all these queue-level tests need.
        Arc::new(Shared::new(unparker))
    }

    fn drained(shared: &Shared) -> Vec<TaskId> {
        let mut out = VecDeque::new();
        shared.drain_into(&mut out);
        out.into()
    }

    #[test]
    fn waking_enqueues_the_task_id() {
        let shared = shared();
        waker_for(7, Arc::clone(&shared)).wake();
        assert_eq!(drained(&shared), vec![7]);
    }

    #[test]
    fn wake_by_ref_can_fire_repeatedly() {
        let shared = shared();
        let waker = waker_for(3, Arc::clone(&shared));
        waker.wake_by_ref();
        waker.wake_by_ref();
        // Still usable afterwards — the strong count was never consumed.
        waker.wake();
        assert_eq!(drained(&shared), vec![3, 3, 3]);
    }

    #[test]
    fn cloned_wakers_are_independent() {
        let shared = shared();
        let waker = waker_for(1, Arc::clone(&shared));
        let clone = waker.clone();
        drop(waker);
        clone.wake(); // the clone still owns a live reference
        assert_eq!(drained(&shared), vec![1]);
    }

    #[test]
    fn waking_from_another_thread_is_sound() {
        // The reason the waker is `Arc`-backed rather than `Rc`-backed.
        let shared = shared();
        let waker = waker_for(42, Arc::clone(&shared));
        std::thread::spawn(move || waker.wake()).join().unwrap();
        assert_eq!(drained(&shared), vec![42]);
    }

    #[test]
    fn main_task_sets_a_flag_instead_of_queueing_an_id() {
        let shared = shared();
        waker_for(MAIN_TASK, Arc::clone(&shared)).wake();
        assert!(drained(&shared).is_empty(), "MAIN_TASK has no slab slot");
        assert!(
            shared.take_main_woken(),
            "the wake must still be observable"
        );
        assert!(!shared.take_main_woken(), "and it is consumed exactly once");
    }

    #[test]
    fn parked_flag_clears_on_the_first_wake_only() {
        let shared = shared();
        shared.set_parked(true);
        shared.schedule(1);
        assert!(!shared.parked.load(Ordering::Acquire), "first wake unparks");
        shared.schedule(2); // no second unpark syscall
        assert_eq!(drained(&shared), vec![1, 2]);
    }

    #[test]
    fn completed_task_reports_done_and_is_not_polled_again() {
        let shared = shared();
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&polls);
        let task = Task::new(
            Box::pin(async move {
                seen.fetch_add(1, Ordering::SeqCst);
            }),
            0,
            shared,
        );
        assert!(
            task.poll(),
            "an immediately-ready future completes on poll 1"
        );
        assert!(task.poll(), "a finished task stays finished");
        assert_eq!(polls.load(Ordering::SeqCst), 1, "future must not run twice");
    }
}
