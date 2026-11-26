//! Mailbox — a bounded, cross-shard message queue.
//!
//! Thread-per-core keeps every task on the shard that spawned it, but state
//! still has an owner: a game room, a cache partition, a session table lives on
//! exactly one shard, and work that concerns it has to *travel* there. A
//! [`MailboxSender`] is how it travels: it is `Send + Sync + Clone`, needs no
//! executor on the sending thread, and [`try_send`](MailboxSender::try_send)
//! never blocks. The [`MailboxReceiver`] stays on the owning shard and awaits
//! messages like any other I/O.
//!
//! # Design
//!
//! - **One `Mutex<VecDeque<T>>`, not a lock-free ring.** The critical sections
//!   are a push or a pop plus a waker swap — nanoseconds, so contention only
//!   shows up when many shards hammer one mailbox at once, and
//!   [`recv_batch`](MailboxReceiver::recv_batch) amortizes the receiver's side
//!   of it across a whole burst. The lock also buys the property that matters
//!   most: correctness is obvious.
//! - **The waker lives under the same lock as the queue.** The receiver checks
//!   for a message and, finding none, registers its [`Waker`] *without
//!   releasing the lock*; a sender pushes and takes the waker under that same
//!   lock. There is no window in which a message can land between "the queue
//!   is empty" and "the waker is registered", so no wakeup can be lost.
//! - **A send takes the waker, it does not clone it.** The first message of a
//!   burst wakes the receiver; the rest find no waker and cost only the push.
//!   The receiver registers again the next time it would sleep.
//! - **Waking is cross-thread safe by construction.** The waker an rt-core
//!   task is polled with is `Arc`-backed (see [`task`](crate::task)): waking it
//!   from another thread queues the task on its own shard and unparks that
//!   shard's reactor through its `mio::Waker`. The sender never touches the
//!   receiving shard's executor directly.
//! - **Bounded.** A full mailbox refuses the message
//!   ([`TrySendError::Full`]) and hands it back. That is the backpressure: the
//!   sender decides whether to drop, retry later, or shed the client — a
//!   mailbox never grows without bound because one shard fell behind.
//!
//! ```
//! use rt_core::{mailbox, Executor};
//!
//! let (tx, mut rx) = mailbox::<u32>(64);
//! let remote = std::thread::spawn(move || {
//!     for n in 0..3 {
//!         tx.try_send(n).unwrap();
//!     }
//!     // `tx` dropped here: the receiver sees the end of the stream.
//! });
//!
//! let ex = Executor::new().unwrap();
//! let total = ex
//!     .block_on(async move {
//!         let mut total = 0;
//!         while let Some(n) = rx.recv().await {
//!             total += n;
//!         }
//!         total
//!     })
//!     .unwrap();
//! remote.join().unwrap();
//! assert_eq!(total, 3);
//! ```

use std::collections::VecDeque;
use std::fmt;
use std::future::poll_fn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

/// Create a mailbox that holds at most `capacity` undelivered messages.
///
/// # Panics
/// If `capacity` is zero — a mailbox that can never hold a message could never
/// deliver one either.
pub fn mailbox<T: Send>(capacity: usize) -> (MailboxSender<T>, MailboxReceiver<T>) {
    assert!(capacity > 0, "mailbox capacity must be at least 1");
    let inner = Arc::new(Inner {
        state: Mutex::new(State {
            // Grow on demand: a mailbox sized for a burst should not reserve
            // the whole burst up front on every shard.
            queue: VecDeque::new(),
            waker: None,
            receiver_alive: true,
        }),
        capacity,
        senders: AtomicUsize::new(1),
    });
    (
        MailboxSender {
            inner: Arc::clone(&inner),
        },
        MailboxReceiver { inner },
    )
}

/// Everything that is read and written together, under one lock.
struct State<T> {
    queue: VecDeque<T>,
    /// The receiver's waker, registered only while it is waiting.
    waker: Option<Waker>,
    /// Cleared when the receiver is dropped; sends fail from then on.
    receiver_alive: bool,
}

struct Inner<T> {
    state: Mutex<State<T>>,
    capacity: usize,
    /// Live [`MailboxSender`]s. Zero plus an empty queue is end-of-stream.
    senders: AtomicUsize,
}

impl<T> Inner<T> {
    /// Take the lock, ignoring poisoning.
    ///
    /// Nothing under the lock can panic midway through an update (a push, a
    /// pop, a waker swap), so a poisoned guard still holds consistent state —
    /// and refusing it would turn one task's panic into a dead mailbox.
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why [`MailboxSender::try_send`] refused a message. The message is handed
/// back either way.
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum TrySendError<T> {
    /// The mailbox already holds `capacity` messages — backpressure.
    Full(T),
    /// The receiver has been dropped; nothing will ever read this message.
    Closed(T),
}

impl<T> TrySendError<T> {
    /// Recover the message that was not sent.
    pub fn into_inner(self) -> T {
        match self {
            Self::Full(msg) | Self::Closed(msg) => msg,
        }
    }

    /// `true` for [`TrySendError::Full`].
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Full(_))
    }

    /// `true` for [`TrySendError::Closed`].
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_))
    }
}

// Hand-written so `T` need not be `Debug` — the same choice std's channels make.
impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("Full(..)"),
            Self::Closed(_) => f.write_str("Closed(..)"),
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("sending on a full mailbox"),
            Self::Closed(_) => f.write_str("sending on a closed mailbox"),
        }
    }
}

impl<T> std::error::Error for TrySendError<T> {}

/// Why [`MailboxReceiver::try_recv`] returned no message.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TryRecvError {
    /// Nothing queued right now; senders are still alive.
    Empty,
    /// Nothing queued, and every sender has been dropped — nothing ever will be.
    Closed,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("receiving on an empty mailbox"),
            Self::Closed => f.write_str("receiving on a closed mailbox"),
        }
    }
}

impl std::error::Error for TryRecvError {}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// The sending half of a [`mailbox`]. Cheap to clone; usable from any thread,
/// with or without an executor running on it.
pub struct MailboxSender<T> {
    inner: Arc<Inner<T>>,
}

impl<T> MailboxSender<T> {
    /// Queue `msg` without blocking, waking the receiver if it is waiting.
    ///
    /// # Errors
    /// [`TrySendError::Full`] when `capacity` messages are already queued, and
    /// [`TrySendError::Closed`] once the receiver is gone. Both return `msg`.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        let waker = {
            let mut state = self.inner.lock();
            if !state.receiver_alive {
                return Err(TrySendError::Closed(msg));
            }
            if state.queue.len() >= self.inner.capacity {
                return Err(TrySendError::Full(msg));
            }
            state.queue.push_back(msg);
            state.waker.take()
        };
        // Wake outside the lock: on the same shard the wake re-enters the
        // executor's queue, and on another it is a syscall — neither belongs
        // inside a critical section the receiver is contending for.
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// Messages queued but not yet received.
    pub fn len(&self) -> usize {
        self.inner.lock().queue.len()
    }

    /// `true` when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most messages this mailbox will hold.
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// `true` once the receiver has been dropped; every send fails from then on.
    pub fn is_closed(&self) -> bool {
        !self.inner.lock().receiver_alive
    }
}

impl<T> Clone for MailboxSender<T> {
    fn clone(&self) -> Self {
        // Relaxed is enough to *add* a sender: the clone is made from a live
        // one, so the count cannot be observed at zero in between.
        self.inner.senders.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> Drop for MailboxSender<T> {
    fn drop(&mut self) {
        if self.inner.senders.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        // Last sender: a receiver parked on an empty queue must learn that
        // nothing more is coming. Taking the waker under the lock orders this
        // against the receiver's check-then-register.
        let waker = self.inner.lock().waker.take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl<T> fmt::Debug for MailboxSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxSender")
            .field("capacity", &self.inner.capacity)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

/// The receiving half of a [`mailbox`].
///
/// There is exactly one, and every receive takes `&mut self`, so it is polled
/// by one task at a time. It is `Send` (for `T: Send`) only so it can be handed
/// to the shard that will own it at startup; after that it is meant to stay
/// there.
pub struct MailboxReceiver<T> {
    inner: Arc<Inner<T>>,
}

impl<T> MailboxReceiver<T> {
    /// Wait for the next message.
    ///
    /// Returns `None` once every [`MailboxSender`] is dropped *and* the queue
    /// has been drained — messages sent before the last sender left are still
    /// delivered.
    pub async fn recv(&mut self) -> Option<T> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// Wait until at least one message is queued, then take up to `max` of
    /// them in a single lock acquisition.
    ///
    /// This is the throughput path: a shard that wakes once and drains a burst
    /// pays one wakeup and one lock for the lot, instead of one of each per
    /// message. An empty `Vec` means the mailbox is closed and drained (or
    /// `max` was zero).
    pub async fn recv_batch(&mut self, max: usize) -> Vec<T> {
        let mut out = Vec::new();
        if max == 0 {
            return out;
        }
        poll_fn(|cx| self.poll_recv_batch(cx, &mut out, max)).await;
        out
    }

    /// Poll for one message — the building block for [`recv`](Self::recv),
    /// for code that composes futures by hand.
    ///
    /// On `Pending` the waker from `cx` is registered and will be woken by the
    /// next send (from any thread) or by the last sender being dropped.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut state = self.inner.lock();
        if let Some(msg) = state.queue.pop_front() {
            return Poll::Ready(Some(msg));
        }
        if self.inner.senders.load(Ordering::Acquire) == 0 {
            return Poll::Ready(None);
        }
        register(&mut state.waker, cx);
        Poll::Pending
    }

    /// Poll for a batch: move up to `max` queued messages into `out`.
    ///
    /// `Ready(n)` with `n > 0` is a batch; `Ready(0)` means closed and drained
    /// (or `max == 0`). `Pending` registers the waker exactly like
    /// [`poll_recv`](Self::poll_recv).
    pub fn poll_recv_batch(
        &mut self,
        cx: &mut Context<'_>,
        out: &mut Vec<T>,
        max: usize,
    ) -> Poll<usize> {
        let mut state = self.inner.lock();
        let taken = drain_locked(&mut state.queue, out, max);
        if taken > 0 || max == 0 {
            return Poll::Ready(taken);
        }
        if self.inner.senders.load(Ordering::Acquire) == 0 {
            return Poll::Ready(0);
        }
        register(&mut state.waker, cx);
        Poll::Pending
    }

    /// Take one message if one is queued, without waiting.
    ///
    /// # Errors
    /// [`TryRecvError::Empty`] when nothing is queued but senders remain;
    /// [`TryRecvError::Closed`] when nothing is queued and none do.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        let mut state = self.inner.lock();
        if let Some(msg) = state.queue.pop_front() {
            return Ok(msg);
        }
        if self.inner.senders.load(Ordering::Acquire) == 0 {
            Err(TryRecvError::Closed)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    /// Move up to `max` queued messages into `out` without waiting. Returns how
    /// many were moved.
    pub fn try_recv_batch(&mut self, out: &mut Vec<T>, max: usize) -> usize {
        drain_locked(&mut self.inner.lock().queue, out, max)
    }

    /// Messages queued but not yet received.
    pub fn len(&self) -> usize {
        self.inner.lock().queue.len()
    }

    /// `true` when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The most messages this mailbox will hold.
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// `true` once every sender has been dropped. Messages already queued are
    /// still delivered.
    pub fn is_closed(&self) -> bool {
        self.inner.senders.load(Ordering::Acquire) == 0
    }
}

impl<T> Drop for MailboxReceiver<T> {
    fn drop(&mut self) {
        // Refuse further sends, and drop anything still queued *outside* the
        // lock — a message's own `Drop` may be arbitrarily expensive.
        let orphaned = {
            let mut state = self.inner.lock();
            state.receiver_alive = false;
            state.waker = None;
            std::mem::take(&mut state.queue)
        };
        drop(orphaned);
    }
}

impl<T> fmt::Debug for MailboxReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxReceiver")
            .field("capacity", &self.inner.capacity)
            .finish_non_exhaustive()
    }
}

/// Store `cx`'s waker, skipping the clone when the one already registered
/// would wake the same task — the common case for a receive loop.
fn register(slot: &mut Option<Waker>, cx: &Context<'_>) {
    match slot {
        Some(existing) if existing.will_wake(cx.waker()) => {}
        _ => *slot = Some(cx.waker().clone()),
    }
}

/// Move up to `max` messages from the front of `queue` into `out`.
fn drain_locked<T>(queue: &mut VecDeque<T>, out: &mut Vec<T>, max: usize) -> usize {
    let n = queue.len().min(max);
    out.extend(queue.drain(..n));
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Executor;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Upper bound on how long any cross-thread test may take. A lost wakeup
    /// parks the executor forever; this turns that into a failure.
    const DEADLINE: Duration = Duration::from_secs(10);

    /// Run `f` on its own thread and fail if it has not finished by
    /// [`DEADLINE`] — a hang is reported instead of hanging the test binary.
    fn with_deadline<R: Send + 'static>(what: &str, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done_tx.send(f());
        });
        done_rx
            .recv_timeout(DEADLINE)
            .unwrap_or_else(|_| panic!("{what}: no result within {DEADLINE:?} — lost wakeup?"))
    }

    fn assert_send_sync<X: Send + Sync>() {}
    fn assert_send<X: Send>() {}

    #[test]
    fn halves_have_the_promised_auto_traits() {
        assert_send_sync::<MailboxSender<String>>();
        assert_send::<MailboxReceiver<String>>();
    }

    #[test]
    fn same_thread_send_then_recv_preserves_order() {
        let (tx, mut rx) = mailbox(8);
        for n in 0..5 {
            tx.try_send(n).unwrap();
        }
        assert_eq!(tx.len(), 5);
        let ex = Executor::new().unwrap();
        let got = ex
            .block_on(async move {
                let mut got = Vec::new();
                for _ in 0..5 {
                    got.push(rx.recv().await.unwrap());
                }
                got
            })
            .unwrap();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_task_on_the_same_shard_wakes_the_receiver() {
        // The receiver parks first; a sibling task's send must wake it.
        let ex = Executor::new().unwrap();
        let (tx, mut rx) = mailbox(4);
        let got = ex
            .block_on(async move {
                crate::spawn(async move {
                    crate::sleep(Duration::from_millis(5)).await;
                    tx.try_send("hello").unwrap();
                });
                rx.recv().await
            })
            .unwrap();
        assert_eq!(got, Some("hello"));
    }

    #[test]
    fn a_plain_thread_wakes_a_receiver_parked_in_block_on() {
        let got = with_deadline("block_on receiver", || {
            let (tx, mut rx) = mailbox(4);
            let sender = std::thread::spawn(move || {
                // Long enough that the receiver is genuinely parked in poll().
                std::thread::sleep(Duration::from_millis(30));
                tx.try_send(42u64).unwrap();
            });
            let ex = Executor::new().unwrap();
            let got = ex.block_on(rx.recv()).unwrap();
            sender.join().unwrap();
            got
        });
        assert_eq!(got, Some(42));
    }

    #[test]
    fn a_plain_thread_wakes_a_spawned_receiver_task() {
        // Same as above, but the receiver is a spawned task, so the wake goes
        // through the shard's ready queue rather than the `block_on` flag.
        let got = with_deadline("spawned receiver", || {
            let (tx, mut rx) = mailbox(4);
            let sender = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                tx.try_send(7u64).unwrap();
            });
            let ex = Executor::new().unwrap();
            let out = Rc::new(Cell::new(None));
            let slot = Rc::clone(&out);
            ex.spawn(async move { slot.set(rx.recv().await) });
            ex.run().unwrap();
            sender.join().unwrap();
            out.get()
        });
        assert_eq!(got, Some(7));
    }

    /// Hammer the register/wake race: a remote sender with no delay, one
    /// message at a time, each one awaited by a receiver that keeps going back
    /// to sleep. Any lost wakeup parks forever and trips the deadline.
    ///
    /// `spawned` picks where the receiver runs: as the `block_on` future (woken
    /// through the executor's main-task flag) or as a spawned task (woken
    /// through the shard's ready queue). Both paths have their own race.
    fn ping_pong(spawned: bool) {
        const ROUNDS: u64 = 20_000;
        let sum = with_deadline("ping-pong", move || {
            let (tx, mut rx) = mailbox(1);
            let (ack_tx, ack_rx) = mpsc::channel::<()>();
            let sender = std::thread::spawn(move || {
                for n in 0..ROUNDS {
                    let mut msg = n;
                    loop {
                        match tx.try_send(msg) {
                            Ok(()) => break,
                            Err(TrySendError::Full(m)) => {
                                msg = m;
                                std::thread::yield_now();
                            }
                            Err(TrySendError::Closed(_)) => return,
                        }
                    }
                    // Wait until the receiver has it, so the next send races the
                    // receiver going back to sleep.
                    if ack_rx.recv().is_err() {
                        return;
                    }
                }
            });
            let receive = async move {
                let mut sum = 0;
                while let Some(n) = rx.recv().await {
                    sum += n;
                    if ack_tx.send(()).is_err() {
                        break;
                    }
                }
                sum
            };
            let ex = Executor::new().unwrap();
            let sum = if spawned {
                let out = Rc::new(Cell::new(0));
                let slot = Rc::clone(&out);
                ex.spawn(async move { slot.set(receive.await) });
                ex.run().unwrap();
                out.get()
            } else {
                ex.block_on(receive).unwrap()
            };
            sender.join().unwrap();
            sum
        });
        assert_eq!(sum, ROUNDS * (ROUNDS - 1) / 2);
    }

    #[test]
    fn many_cross_thread_wakes_of_block_on_are_never_lost() {
        ping_pong(false);
    }

    #[test]
    fn many_cross_thread_wakes_of_a_spawned_task_are_never_lost() {
        ping_pong(true);
    }

    #[test]
    fn a_full_mailbox_refuses_and_returns_the_message() {
        let (tx, mut rx) = mailbox(2);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        let err = tx.try_send(3).unwrap_err();
        assert!(err.is_full());
        assert_eq!(err.into_inner(), 3, "the refused message comes back");
        // Draining one frees exactly one slot.
        assert_eq!(rx.try_recv(), Ok(1));
        tx.try_send(3).unwrap();
        assert!(tx.try_send(4).unwrap_err().is_full());
    }

    #[test]
    fn dropping_the_receiver_closes_the_mailbox() {
        let (tx, rx) = mailbox(2);
        tx.try_send(String::from("queued")).unwrap();
        drop(rx);
        assert!(tx.is_closed());
        let err = tx.try_send(String::from("late")).unwrap_err();
        assert!(err.is_closed());
        assert_eq!(err.into_inner(), "late");
        assert_eq!(tx.len(), 0, "queued messages are dropped with the receiver");
    }

    #[test]
    fn recv_returns_none_after_every_sender_is_dropped_and_the_queue_drains() {
        let (tx, mut rx) = mailbox(4);
        let tx2 = tx.clone();
        tx.try_send(1).unwrap();
        tx2.try_send(2).unwrap();
        drop(tx);
        assert!(!rx.is_closed(), "one sender is still alive");
        drop(tx2);
        assert!(rx.is_closed());
        let ex = Executor::new().unwrap();
        let got = ex
            .block_on(async move {
                let mut got = Vec::new();
                while let Some(n) = rx.recv().await {
                    got.push(n);
                }
                assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
                got
            })
            .unwrap();
        assert_eq!(got, vec![1, 2], "messages sent before closing still arrive");
    }

    #[test]
    fn dropping_the_last_sender_wakes_a_parked_receiver() {
        let got = with_deadline("close wake", || {
            let (tx, mut rx) = mailbox::<u8>(4);
            let closer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                drop(tx);
            });
            let ex = Executor::new().unwrap();
            let got = ex.block_on(rx.recv()).unwrap();
            closer.join().unwrap();
            got
        });
        assert_eq!(got, None);
    }

    #[test]
    fn try_recv_distinguishes_empty_from_closed() {
        let (tx, mut rx) = mailbox::<u8>(1);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(tx);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    }

    #[test]
    fn recv_batch_drains_a_burst_up_to_max() {
        let (tx, mut rx) = mailbox(16);
        for n in 0..10 {
            tx.try_send(n).unwrap();
        }
        let ex = Executor::new().unwrap();
        let (first, second, third) = ex
            .block_on(async move {
                let first = rx.recv_batch(4).await;
                let second = rx.recv_batch(100).await;
                drop(tx);
                let third = rx.recv_batch(100).await;
                (first, second, third)
            })
            .unwrap();
        assert_eq!(first, vec![0, 1, 2, 3]);
        assert_eq!(second, vec![4, 5, 6, 7, 8, 9]);
        assert!(third.is_empty(), "empty batch = closed and drained");
    }

    #[test]
    fn recv_batch_waits_for_a_remote_burst() {
        let total = with_deadline("batch receiver", || {
            const N: usize = 10_000;
            let (tx, mut rx) = mailbox(256);
            let sender = std::thread::spawn(move || {
                let mut sent = 0;
                while sent < N {
                    match tx.try_send(sent) {
                        Ok(()) => sent += 1,
                        Err(TrySendError::Full(_)) => std::thread::yield_now(),
                        Err(TrySendError::Closed(_)) => return,
                    }
                }
            });
            let ex = Executor::new().unwrap();
            let received = ex
                .block_on(async move {
                    let mut expected = 0;
                    loop {
                        let batch = rx.recv_batch(64).await;
                        if batch.is_empty() {
                            return expected;
                        }
                        assert!(batch.len() <= 64);
                        for n in batch {
                            assert_eq!(n, expected, "order is preserved across batches");
                            expected += 1;
                        }
                    }
                })
                .unwrap();
            sender.join().unwrap();
            received
        });
        assert_eq!(total, 10_000);
    }

    #[test]
    fn try_recv_batch_does_not_wait() {
        let (tx, mut rx) = mailbox(8);
        let mut out = Vec::new();
        assert_eq!(rx.try_recv_batch(&mut out, 8), 0);
        tx.try_send('a').unwrap();
        tx.try_send('b').unwrap();
        assert_eq!(rx.try_recv_batch(&mut out, 8), 2);
        assert_eq!(out, vec!['a', 'b']);
        assert!(rx.is_empty());
    }

    #[test]
    #[should_panic(expected = "at least 1")]
    fn zero_capacity_is_rejected() {
        let _ = mailbox::<u8>(0);
    }
}
