//! Shutdown — a one-way signal every shard can wait on.
//!
//! A server is shut down from *outside* the runtime: a signal handler, an admin
//! endpoint, a test harness. That thread is not the thread the shards run on, so
//! the signal has to cross threads while the waiters — accept loops, connection
//! tasks — stay `!Send` on their own shard.
//!
//! [`Shutdown`] is therefore `Arc`-backed and its waiters register a plain
//! [`Waker`]. Waking one goes through the same path a `spawn_blocking` result or
//! a timer takes: schedule the task on its shard, then unpark that shard's
//! reactor through its `mio::Waker`. A parked shard wakes on the syscall it was
//! already sleeping in; no polling, no dedicated thread.
//!
//! The signal is **latching**: once triggered it stays triggered, and a waiter
//! registered afterwards completes on its first poll. That is what makes it safe
//! to hand a clone to a connection task spawned mid-shutdown — it cannot miss
//! the edge, because there is no edge to miss, only a state.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Registered waiters. Slots are reused so a long-lived server whose connection
/// tasks come and go does not grow this vector without bound.
#[derive(Default)]
struct Slots {
    entries: Vec<Option<Waker>>,
    free: Vec<usize>,
}

impl Slots {
    fn insert(&mut self, waker: Waker) -> usize {
        match self.free.pop() {
            Some(index) => {
                self.entries[index] = Some(waker);
                index
            }
            None => {
                self.entries.push(Some(waker));
                self.entries.len() - 1
            }
        }
    }

    fn replace(&mut self, index: usize, waker: Waker) {
        if let Some(slot) = self.entries.get_mut(index) {
            *slot = Some(waker);
        }
    }

    fn remove(&mut self, index: usize) {
        if let Some(slot) = self.entries.get_mut(index) {
            if slot.take().is_some() {
                self.free.push(index);
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len() - self.free.len()
    }
}

struct State {
    triggered: AtomicBool,
    waiters: Mutex<Slots>,
}

impl State {
    /// Take the lock, ignoring poisoning.
    ///
    /// A panicking waiter leaves the waker list intact — nothing here can be
    /// observed half-updated — so refusing the lock afterwards would turn one
    /// task's panic into a server that can no longer be shut down.
    fn waiters(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.waiters.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A latching shutdown signal, cloneable and `Send`.
///
/// ```
/// use rt_core::{Executor, Shutdown};
///
/// let shutdown = Shutdown::new();
/// let trigger = shutdown.clone();
/// let ex = Executor::new().unwrap();
/// ex.block_on(async move {
///     trigger.trigger();
///     shutdown.wait().await; // already triggered → returns at once
/// })
/// .unwrap();
/// ```
#[derive(Clone)]
pub struct Shutdown {
    state: Arc<State>,
}

impl Shutdown {
    /// A signal that has not fired.
    pub fn new() -> Self {
        Self {
            state: Arc::new(State {
                triggered: AtomicBool::new(false),
                waiters: Mutex::new(Slots::default()),
            }),
        }
    }

    /// Fire the signal, waking every waiter. Idempotent: later calls are no-ops.
    pub fn trigger(&self) {
        // `Release` publishes everything the triggering thread did beforehand
        // (a reason string, a flag) to the shards that observe the signal.
        if self.state.triggered.swap(true, Ordering::Release) {
            return;
        }
        // Collect under the lock, wake outside it: a waker may run inline and
        // re-enter this signal (a task that drops its waiter as it finishes).
        let wakers: Vec<Waker> = {
            let mut waiters = self.state.waiters();
            let wakers = waiters.entries.drain(..).flatten().collect();
            // The freed indices refer to slots that no longer exist; leaving
            // them would make `len()` underflow.
            waiters.free.clear();
            wakers
        };
        for waker in wakers {
            waker.wake();
        }
    }

    /// Whether the signal has fired — the cheap check for code that has
    /// somewhere else to be (deciding whether to keep a connection alive).
    pub fn is_triggered(&self) -> bool {
        self.state.triggered.load(Ordering::Acquire)
    }

    /// Complete when the signal fires, immediately if it already has.
    pub fn wait(&self) -> Waiting {
        Waiting {
            state: Arc::clone(&self.state),
            slot: None,
        }
    }

    /// Waiters currently registered — for tests asserting slots are released.
    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.state.waiters().len()
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Shutdown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shutdown")
            .field("triggered", &self.is_triggered())
            .finish()
    }
}

/// Future returned by [`Shutdown::wait`].
pub struct Waiting {
    state: Arc<State>,
    /// Assigned on the first poll — a `Waiting` that is never awaited registers
    /// nothing and so costs nothing.
    slot: Option<usize>,
}

impl Future for Waiting {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.state.triggered.load(Ordering::Acquire) {
            self.release();
            return Poll::Ready(());
        }
        let waker = cx.waker().clone();
        match self.slot {
            Some(index) => self.state.waiters().replace(index, waker),
            None => {
                let index = self.state.waiters().insert(waker);
                self.slot = Some(index);
            }
        }
        // Re-check after registering: a trigger between the first load and the
        // insert drained a list this waker was not in yet, and would otherwise
        // leave it parked forever.
        if self.state.triggered.load(Ordering::Acquire) {
            self.release();
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Waiting {
    /// Give the slot back, so a connection that finishes normally does not leave
    /// a dead waker behind for the life of the server.
    fn release(&mut self) {
        if let Some(index) = self.slot.take() {
            self.state.waiters().remove(index);
        }
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.release();
    }
}

/// Run `future`, giving up if `shutdown` fires first.
///
/// `Some(output)` if the future finished, `None` if the signal won. Shaped like
/// [`timeout`](crate::timeout) — same "race against something that ends the
/// wait" idea, with a signal instead of a deadline.
///
/// The future is polled first, so work that completes in the same tick as the
/// signal is reported as done rather than cancelled.
pub fn until_shutdown<F: Future>(shutdown: &Shutdown, future: F) -> UntilShutdown<F> {
    UntilShutdown {
        future,
        waiting: shutdown.wait(),
    }
}

/// Future returned by [`until_shutdown`].
pub struct UntilShutdown<F> {
    future: F,
    waiting: Waiting,
}

impl<F: Future> Future for UntilShutdown<F> {
    type Output = Option<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: structural pin projection. `UntilShutdown` never moves out of
        // either field and has no `Drop` that could observe a moved field, so
        // re-pinning `future` in place is sound.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: `this` is pinned, so `this.future` is too and cannot move.
        let future = unsafe { Pin::new_unchecked(&mut this.future) };
        if let Poll::Ready(output) = future.poll(cx) {
            return Poll::Ready(Some(output));
        }
        match Pin::new(&mut this.waiting).poll(cx) {
            Poll::Ready(()) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Executor;
    use std::time::Duration;

    #[test]
    fn an_already_triggered_signal_completes_at_once() {
        let ex = Executor::new().unwrap();
        let shutdown = Shutdown::new();
        shutdown.trigger();
        assert!(shutdown.is_triggered());
        ex.block_on(shutdown.wait()).unwrap();
    }

    #[test]
    fn a_waiter_is_woken_when_the_signal_fires() {
        let ex = Executor::new().unwrap();
        let shutdown = Shutdown::new();
        let trigger = shutdown.clone();
        ex.block_on(async move {
            crate::spawn(async move {
                crate::sleep(Duration::from_millis(20)).await;
                trigger.trigger();
            });
            shutdown.wait().await;
        })
        .unwrap();
    }

    #[test]
    fn a_trigger_from_another_thread_unparks_the_shard() {
        // The whole cross-thread path: Arc state → task waker → shared queue →
        // mio unparker → the `poll()` the shard is asleep in returns.
        let ex = Executor::new().unwrap();
        let shutdown = Shutdown::new();
        let trigger = shutdown.clone();
        let started = std::time::Instant::now();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            trigger.trigger();
        });
        ex.block_on(shutdown.wait()).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the shard slept through the signal"
        );
        thread.join().unwrap();
    }

    #[test]
    fn until_shutdown_returns_the_output_when_the_future_wins() {
        let ex = Executor::new().unwrap();
        let shutdown = Shutdown::new();
        let out = ex.block_on(until_shutdown(&shutdown, async { 42 })).unwrap();
        assert_eq!(out, Some(42));
    }

    #[test]
    fn until_shutdown_gives_up_on_a_future_that_never_completes() {
        let ex = Executor::new().unwrap();
        let shutdown = Shutdown::new();
        let trigger = shutdown.clone();
        let out = ex
            .block_on(async move {
                crate::spawn(async move {
                    crate::sleep(Duration::from_millis(20)).await;
                    trigger.trigger();
                });
                until_shutdown(&shutdown, std::future::pending::<()>()).await
            })
            .unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn a_finished_waiter_releases_its_slot() {
        // Every connection waits on this signal. If a waiter that completes
        // normally left its waker behind, the list would grow for the life of
        // the server — one dead entry per request served.
        let ex = Executor::new().unwrap();
        let shutdown = Shutdown::new();
        ex.block_on(async {
            for _ in 0..100 {
                let _ = until_shutdown(&shutdown, async {}).await;
            }
        })
        .unwrap();
        assert_eq!(shutdown.waiter_count(), 0);
    }

    #[test]
    fn triggering_twice_is_harmless() {
        let shutdown = Shutdown::new();
        shutdown.trigger();
        shutdown.trigger();
        assert!(shutdown.is_triggered());
    }

    #[test]
    fn a_waiter_registered_after_the_trigger_still_completes() {
        // Connection tasks are spawned continuously; one that starts a
        // microsecond after the signal must not wait forever for an edge that
        // has already passed.
        let ex = Executor::new().unwrap();
        let shutdown = Shutdown::new();
        shutdown.trigger();
        let out = ex
            .block_on(until_shutdown(&shutdown, std::future::pending::<()>()))
            .unwrap();
        assert_eq!(out, None);
    }
}
