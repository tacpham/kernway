//! Timers — `sleep` and `timeout`.
//!
//! A shard sleeps inside `mio::Poll::poll`, which already takes a timeout, so
//! timers cost nothing extra to wait on: the executor simply parks until the
//! earliest deadline instead of indefinitely. Deadlines live in a min-heap;
//! expired ones are drained on every loop iteration.
//!
//! Cancellation is lazy — a dropped [`Sleep`] removes its waker but leaves the
//! heap entry, which is discarded when it surfaces. That keeps `Drop` O(log n)
//! at worst instead of scanning the heap.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// Identifies one registered deadline.
pub(crate) type TimerId = u64;

/// A deadline waiting to fire. Ordered by time, then id for a stable order.
#[derive(PartialEq, Eq)]
struct Entry {
    deadline: Instant,
    id: TimerId,
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline
            .cmp(&other.deadline)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Per-shard timer wheel.
#[derive(Default)]
pub(crate) struct Timers {
    /// `Reverse` turns the max-heap into a min-heap on deadline.
    heap: BinaryHeap<Reverse<Entry>>,
    /// Live timers only — a cancelled id is absent here even while its heap
    /// entry lingers.
    wakers: HashMap<TimerId, Waker>,
    next_id: TimerId,
}

impl Timers {
    /// Reserve an id for a new timer.
    pub(crate) fn allocate(&mut self) -> TimerId {
        self.next_id += 1;
        self.next_id
    }

    /// Register (or re-register) `id` to fire at `deadline`.
    pub(crate) fn park(&mut self, id: TimerId, deadline: Instant, waker: Waker) {
        if self.wakers.insert(id, waker).is_none() {
            // Only push once per id; a re-poll just replaces the waker.
            self.heap.push(Reverse(Entry { deadline, id }));
        }
    }

    /// Whether any timer entry is pending — a cheap check the executor makes
    /// on every round of a busy run queue, before paying for `Instant::now`.
    pub(crate) fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Forget `id`. Its heap entry is dropped when it reaches the top.
    pub(crate) fn cancel(&mut self, id: TimerId) {
        self.wakers.remove(&id);
    }

    /// When the executor must next wake up, if any timer is pending.
    ///
    /// Skips entries whose timer was cancelled, so a heap full of dead
    /// deadlines cannot hold the shard awake.
    pub(crate) fn next_deadline(&mut self) -> Option<Instant> {
        while let Some(Reverse(entry)) = self.heap.peek() {
            if self.wakers.contains_key(&entry.id) {
                return Some(entry.deadline);
            }
            self.heap.pop();
        }
        None
    }

    /// Remove every timer due at or before `now` and return their wakers.
    pub(crate) fn fire_expired(&mut self, now: Instant, out: &mut Vec<Waker>) {
        while let Some(Reverse(entry)) = self.heap.peek() {
            if entry.deadline > now {
                return;
            }
            let Reverse(entry) = self.heap.pop().expect("peeked");
            if let Some(waker) = self.wakers.remove(&entry.id) {
                out.push(waker);
            }
        }
    }
}

/// Future returned by [`sleep`] / [`sleep_until`].
pub struct Sleep {
    deadline: Instant,
    /// Assigned on the first poll — a `Sleep` that is never awaited registers
    /// nothing and so costs nothing.
    id: Option<TimerId>,
}

impl Sleep {
    /// The instant this sleep completes.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            // Fired: drop the registration so a late wake cannot resurrect it.
            if let Some(id) = self.id.take() {
                crate::executor::with_timers(|t| t.cancel(id));
            }
            return Poll::Ready(());
        }
        let id = match self.id {
            Some(id) => id,
            None => {
                let id = crate::executor::with_timers(|t| t.allocate());
                self.id = Some(id);
                id
            }
        };
        let deadline = self.deadline;
        let waker = cx.waker().clone();
        crate::executor::with_timers(|t| t.park(id, deadline, waker));
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            // The executor may already be gone (shutdown) — then there is
            // nothing holding the registration either.
            let _ = crate::executor::try_with_timers(|t| t.cancel(id));
        }
    }
}

/// Complete after `duration`.
///
/// # Panics
/// On first poll, if no executor is running on this thread.
pub fn sleep(duration: Duration) -> Sleep {
    sleep_until(Instant::now() + duration)
}

/// Complete at `deadline`.
pub fn sleep_until(deadline: Instant) -> Sleep {
    Sleep { deadline, id: None }
}

/// `future` did not finish in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("operation timed out")
    }
}

impl std::error::Error for Elapsed {}

impl From<Elapsed> for std::io::Error {
    fn from(_: Elapsed) -> Self {
        std::io::Error::new(std::io::ErrorKind::TimedOut, "operation timed out")
    }
}

/// Future returned by [`timeout`].
pub struct Timeout<F> {
    future: F,
    sleep: Sleep,
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pin projection. `Timeout` never moves out of either
        // field, is not `Unpin`-dependent, and has no `Drop` that could observe
        // a moved field — so re-pinning `future` in place is sound.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: `this` is pinned, so `this.future` is too and cannot move.
        let future = unsafe { Pin::new_unchecked(&mut this.future) };
        if let Poll::Ready(output) = future.poll(cx) {
            return Poll::Ready(Ok(output));
        }
        // The inner future is checked first, so work that completes in the same
        // tick as the deadline is reported as success rather than a timeout.
        match Pin::new(&mut this.sleep).poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Run `future`, giving up after `duration`.
///
/// ```
/// # use rt_core::{Executor, time};
/// # use std::time::Duration;
/// let ex = Executor::new().unwrap();
/// let out = ex.block_on(async {
///     time::timeout(Duration::from_millis(10), std::future::pending::<()>()).await
/// }).unwrap();
/// assert!(out.is_err());
/// ```
pub fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F> {
    Timeout {
        future,
        sleep: sleep(duration),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Executor;

    #[test]
    fn sleep_waits_at_least_its_duration() {
        let ex = Executor::new().unwrap();
        let started = Instant::now();
        ex.block_on(sleep(Duration::from_millis(50))).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn an_already_past_deadline_completes_immediately() {
        let ex = Executor::new().unwrap();
        let started = Instant::now();
        ex.block_on(sleep_until(Instant::now() - Duration::from_secs(1)))
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn timers_fire_in_deadline_order_not_registration_order() {
        let ex = Executor::new().unwrap();
        let order = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));

        for (label, ms) in [("late", 60), ("early", 10), ("middle", 30)] {
            let order = std::rc::Rc::clone(&order);
            ex.spawn(async move {
                sleep(Duration::from_millis(ms)).await;
                order.borrow_mut().push(label);
            });
        }
        ex.run().unwrap();
        assert_eq!(*order.borrow(), vec!["early", "middle", "late"]);
    }

    #[test]
    fn timeout_returns_the_value_when_the_future_wins() {
        let ex = Executor::new().unwrap();
        let out = ex
            .block_on(timeout(Duration::from_secs(5), async { 42 }))
            .unwrap();
        assert_eq!(out, Ok(42));
    }

    #[test]
    fn timeout_elapses_on_a_future_that_never_completes() {
        let ex = Executor::new().unwrap();
        let started = Instant::now();
        let out = ex
            .block_on(timeout(
                Duration::from_millis(30),
                std::future::pending::<()>(),
            ))
            .unwrap();
        assert_eq!(out, Err(Elapsed));
        assert!(started.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn a_dropped_sleep_does_not_hold_the_executor_awake() {
        // The timeout drops its Sleep on the winning path; a leaked
        // registration would keep the shard waking up for a dead deadline.
        let ex = Executor::new().unwrap();
        ex.block_on(async {
            let _ = timeout(Duration::from_secs(30), async {}).await;
        })
        .unwrap();
        let started = Instant::now();
        ex.block_on(sleep(Duration::from_millis(20))).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a cancelled 30s timer must not be waited on"
        );
    }

    #[test]
    fn many_concurrent_timers_all_fire() {
        let ex = Executor::new().unwrap();
        let done = std::rc::Rc::new(std::cell::Cell::new(0));
        for i in 0..50 {
            let done = std::rc::Rc::clone(&done);
            ex.spawn(async move {
                sleep(Duration::from_millis(i % 10)).await;
                done.set(done.get() + 1);
            });
        }
        ex.run().unwrap();
        assert_eq!(done.get(), 50);
    }

    #[test]
    fn cancelled_entries_do_not_block_the_next_deadline() {
        let mut timers = Timers::default();
        let dead = timers.allocate();
        let live = timers.allocate();
        let now = Instant::now();
        let waker = std::task::Waker::noop().clone();
        timers.park(dead, now + Duration::from_millis(1), waker.clone());
        timers.park(live, now + Duration::from_secs(10), waker);
        timers.cancel(dead);
        // The soonest *live* deadline is the 10s one, not the cancelled 1ms.
        let next = timers.next_deadline().unwrap();
        assert!(next > now + Duration::from_secs(9));
    }
}
