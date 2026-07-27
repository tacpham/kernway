//! `spawn_blocking` — move blocking work off the shard.
//!
//! A shard's executor is single-threaded: one blocking call (a synchronous DB
//! driver, a file read, a long CPU crunch) stalls *every* connection on that
//! core. This offloads the closure to a shared pool and wakes the awaiting task
//! through its `Waker` — the same cross-thread path `rt-net` uses, which is why
//! the waker had to be `Arc`-backed.
//!
//! Spring's `CompletableFuture.supplyAsync(executor)` plays the same role.

use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};

/// A unit of work for the pool.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// The pool is global rather than per-shard: blocking work has no affinity, and
/// a per-shard pool would idle N times as many threads.
static POOL: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();

/// Threads to keep for blocking work. Generous relative to core count because
/// these threads are expected to be *blocked*, not running.
fn pool_size() -> usize {
    (crate::sys::default_shard_count() * 4).clamp(4, 512)
}

fn pool() -> &'static Mutex<Sender<Job>> {
    POOL.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..pool_size() {
            let rx = Arc::clone(&rx);
            let _ = std::thread::Builder::new()
                .name(format!("kernway-blocking-{i}"))
                .spawn(move || loop {
                    // Hold the receiver lock only long enough to take one job,
                    // so workers do not serialise on each other's execution.
                    let job = {
                        let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
                        guard.recv()
                    };
                    match job {
                        Ok(job) => job(),
                        Err(_) => break, // sender dropped — process shutting down
                    }
                });
        }
        Mutex::new(tx)
    })
}

/// Shared slot between the worker thread and the awaiting task.
struct Slot<T> {
    value: Option<T>,
    waker: Option<Waker>,
    /// Set even when the closure panics, so the future never hangs.
    finished: bool,
}

/// The future returned by [`spawn_blocking`].
pub struct Blocking<T> {
    slot: Arc<Mutex<Slot<T>>>,
}

impl<T> Future for Blocking<T> {
    /// `None` if the closure panicked — the panic is contained on the worker
    /// thread rather than being resumed on the shard, which would take down
    /// every other connection on that core.
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.finished {
            return Poll::Ready(slot.value.take());
        }
        // Re-park under the lock: a worker finishing right here would otherwise
        // store its result and find no waker to call.
        slot.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Run `f` on the blocking pool and await its result.
///
/// Use for synchronous DB drivers, blocking file I/O, and CPU-heavy work.
///
/// # Example
/// ```
/// # use rt_core::{Executor, spawn_blocking};
/// let ex = Executor::new().unwrap();
/// let n = ex.block_on(async { spawn_blocking(|| 2 + 2).await }).unwrap();
/// assert_eq!(n, Some(4));
/// ```
pub fn spawn_blocking<F, T>(f: F) -> Blocking<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let slot = Arc::new(Mutex::new(Slot {
        value: None,
        waker: None,
        finished: false,
    }));
    let worker_slot = Arc::clone(&slot);

    let job: Job = Box::new(move || {
        // `AssertUnwindSafe`: the slot is only written after the closure has run
        // and is not observable in a torn state.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok();
        let waker = {
            let mut slot = worker_slot.lock().unwrap_or_else(|e| e.into_inner());
            slot.value = result;
            slot.finished = true;
            slot.waker.take()
        };
        // Wake outside the lock — the woken task may poll immediately on
        // another thread and would otherwise contend for a lock we still hold.
        if let Some(waker) = waker {
            waker.wake();
        }
    });

    let sent = pool()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(job)
        .is_ok();
    if !sent {
        // The pool is gone (shutdown); complete as a panic-equivalent rather
        // than leaving the caller awaiting forever.
        let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        guard.finished = true;
    }

    Blocking { slot }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Executor;
    use std::time::Duration;

    #[test]
    fn returns_the_closure_result() {
        let ex = Executor::new().unwrap();
        let out = ex
            .block_on(async { spawn_blocking(|| 21 * 2).await })
            .unwrap();
        assert_eq!(out, Some(42));
    }

    #[test]
    fn a_slow_job_wakes_the_parked_executor() {
        // The executor has nothing else to run, so it parks; only the pool
        // thread's `waker.wake()` can bring it back.
        let ex = Executor::new().unwrap();
        let out = ex
            .block_on(async {
                spawn_blocking(|| {
                    std::thread::sleep(Duration::from_millis(30));
                    "slow"
                })
                .await
            })
            .unwrap();
        assert_eq!(out, Some("slow"));
    }

    #[test]
    fn a_panicking_job_yields_none_instead_of_hanging() {
        let ex = Executor::new().unwrap();
        let out = ex
            .block_on(async { spawn_blocking(|| panic!("boom")).await })
            .unwrap();
        assert_eq!(out, None::<()>);
    }

    #[test]
    fn many_jobs_run_concurrently_not_serially() {
        let ex = Executor::new().unwrap();
        let started = std::time::Instant::now();
        ex.block_on(async {
            let jobs: Vec<_> = (0..4)
                .map(|i| {
                    spawn_blocking(move || {
                        std::thread::sleep(Duration::from_millis(40));
                        i
                    })
                })
                .collect();
            for job in jobs {
                assert!(job.await.is_some());
            }
        })
        .unwrap();
        // Serial execution would need ~160ms; allow a wide margin for CI noise.
        assert!(
            started.elapsed() < Duration::from_millis(140),
            "jobs appear to be serialised: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn result_is_available_even_if_polled_late() {
        // Completing before the first poll must not lose the value.
        let ex = Executor::new().unwrap();
        let out = ex
            .block_on(async {
                let job = spawn_blocking(|| 7);
                std::thread::sleep(Duration::from_millis(50)); // job finishes first
                job.await
            })
            .unwrap();
        assert_eq!(out, Some(7));
    }
}
