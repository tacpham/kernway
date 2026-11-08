//! Single-flight: coalesce concurrent misses for the same key so the loader runs
//! once, not once per caller.
//!
//! The problem (cache stampede): a hot key expires, N requests miss it in the
//! same instant, and all N run the same expensive load against the source. This
//! lets the *first* caller compute while the rest wait on a per-key gate, then
//! read the value the winner stored — N loads collapse to 1.
//!
//! Scope: this coalesces within **one process/node**. Coalescing across nodes
//! needs a distributed lease (see `docs/cache-roadmap.md`).

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

/// A per-key coalescing registry.
///
/// Each key maps to a gate (`Arc<Mutex<()>>`); the caller that creates it runs
/// the load, every other caller blocks on the same gate and then re-reads the
/// cache. Gates are retired once no caller still holds them, so the registry
/// does not grow without bound.
pub struct SingleFlight<K> {
    inflight: Mutex<HashMap<K, Arc<Mutex<()>>>>,
}

impl<K> SingleFlight<K> {
    /// A new, empty registry.
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Lock the registry, recovering from poisoning (a panicking loader must not
    /// disable coalescing for every other key).
    fn lock(&self) -> MutexGuard<'_, HashMap<K, Arc<Mutex<()>>>> {
        self.inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

}

impl<K> SingleFlight<K>
where
    K: Eq + Hash + Clone,
{
    /// The gate for `key`, creating it if this is the first caller.
    fn gate(&self, key: &K) -> Arc<Mutex<()>> {
        self.lock()
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Drop the gate from the registry once only this call still references it.
    ///
    /// Held under the registry lock so no `gate()` can clone it concurrently: a
    /// strong count of 2 means exactly "the map slot + our local handle", i.e. no
    /// waiter needs it anymore. A higher count means a waiter is still inside —
    /// leave it; the last one out retires it.
    fn retire(&self, key: &K, gate: &Arc<Mutex<()>>) {
        let mut map = self.lock();
        if let Some(cur) = map.get(key) {
            if Arc::ptr_eq(cur, gate) && Arc::strong_count(cur) <= 2 {
                map.remove(key);
            }
        }
    }

    /// Run `compute` for `key` under single-flight.
    ///
    /// `peek` reads the cache; it is checked on the fast path and again after the
    /// gate is acquired (by then the winner may have stored the value). `compute`
    /// is the load **and store** — it must place the value in the cache before
    /// returning, so that a waiter's second `peek` is a hit. Only one caller per
    /// key runs `compute` at a time; if it errors nothing is stored and the next
    /// waiter becomes the new runner (errors are not cached).
    pub fn run<V, P, C, E>(&self, key: &K, peek: P, compute: C) -> Result<V, E>
    where
        P: Fn() -> Option<V>,
        C: FnOnce() -> Result<V, E>,
    {
        // Fast path: already cached, no gate needed.
        if let Some(v) = peek() {
            return Ok(v);
        }
        let gate = self.gate(key);
        let result = {
            let _held = gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // The winner may have filled the cache while we waited for the gate.
            if let Some(v) = peek() {
                Ok(v)
            } else {
                compute()
            }
        };
        self.retire(key, &gate);
        result
    }
}

impl<K> Default for SingleFlight<K> {
    fn default() -> Self {
        Self::new()
    }
}

// --- Async single-flight ---------------------------------------------------

/// A per-key gate waiters `.await` instead of blocking. `done` is checked under
/// the `wakers` lock so a `finish` that races a registering waiter can never
/// lose the wakeup.
struct AsyncGate {
    done: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

impl AsyncGate {
    fn new() -> Arc<Self> {
        Arc::new(Self { done: AtomicBool::new(false), wakers: Mutex::new(Vec::new()) })
    }

    fn finish(&self) {
        let mut wakers = self.wakers.lock().unwrap_or_else(|p| p.into_inner());
        self.done.store(true, Ordering::Release);
        let drained = std::mem::take(&mut *wakers);
        drop(wakers);
        for w in drained {
            w.wake();
        }
    }
}

struct WaitGate {
    gate: Arc<AsyncGate>,
}

impl Future for WaitGate {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Register under the lock, then check `done` under the same lock, so a
        // concurrent `finish()` either drains our waker or we observe `done`.
        let mut wakers = self.gate.wakers.lock().unwrap_or_else(|p| p.into_inner());
        if self.gate.done.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        wakers.push(cx.waker().clone());
        Poll::Pending
    }
}

/// The async sibling of [`SingleFlight`]: coalesce concurrent misses for the
/// same key when the loader is **async**.
///
/// Waiters **`.await`** a per-key gate instead of blocking the thread, so a slow
/// (network) loader never freezes a reactor shard. It stays `Send + Sync` (gates
/// are `Arc`-backed, wakers cross threads), so it works for a cache shared across
/// shards — coalescing across both tasks and threads.
pub struct AsyncSingleFlight<K> {
    inflight: Mutex<HashMap<K, Arc<AsyncGate>>>,
}

impl<K> AsyncSingleFlight<K> {
    /// A new, empty registry.
    pub fn new() -> Self {
        Self { inflight: Mutex::new(HashMap::new()) }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<K, Arc<AsyncGate>>> {
        self.inflight.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl<K> AsyncSingleFlight<K>
where
    K: Eq + Hash + Clone,
{
    /// Run `compute` for `key` under single-flight.
    ///
    /// `peek` reads the cache (checked up front and again after the gate). Only
    /// one caller per key runs `compute` at a time; the rest await and then
    /// re-`peek`. `compute` must store its value before resolving so a waiter's
    /// re-`peek` hits. Errors are not cached — a waiter whose winner failed
    /// becomes the next runner.
    pub async fn run<V, P, C, Fut, E>(&self, key: &K, peek: P, compute: C) -> Result<V, E>
    where
        P: Fn() -> Option<V>,
        C: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let mut compute = Some(compute);
        loop {
            if let Some(v) = peek() {
                return Ok(v);
            }
            // Claim or join the in-flight gate for this key, atomically.
            let (gate, winner) = {
                let mut map = self.lock();
                if let Some(g) = map.get(key) {
                    (Arc::clone(g), false)
                } else {
                    let g = AsyncGate::new();
                    map.insert(key.clone(), Arc::clone(&g));
                    (g, true)
                }
            };

            if winner {
                let compute = compute.take().expect("compute runs at most once");
                let result = compute().await;
                self.lock().remove(key);
                gate.finish();
                return result;
            }

            WaitGate { gate }.await;
            if let Some(v) = peek() {
                return Ok(v);
            }
            // Winner errored (nothing stored) — loop and try to become the runner.
        }
    }
}

impl<K> Default for AsyncSingleFlight<K> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::time::Duration;

    #[test]
    fn concurrent_misses_run_compute_once() {
        // A shared "cache" the compute closure fills.
        let store: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let loads = Arc::new(AtomicUsize::new(0));
        let flight: Arc<SingleFlight<&'static str>> = Arc::new(SingleFlight::new());
        let barrier = Arc::new(Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (store, loads, flight, barrier) = (
                    Arc::clone(&store),
                    Arc::clone(&loads),
                    Arc::clone(&flight),
                    Arc::clone(&barrier),
                );
                std::thread::spawn(move || {
                    barrier.wait(); // maximise the race
                    flight
                        .run::<u64, _, _, ()>(
                            &"k",
                            || *store.lock().unwrap(),
                            || {
                                loads.fetch_add(1, Ordering::SeqCst);
                                std::thread::sleep(Duration::from_millis(20));
                                let v = 42;
                                *store.lock().unwrap() = Some(v); // store BEFORE returning
                                Ok(v)
                            },
                        )
                        .unwrap()
                })
            })
            .collect();

        for h in handles {
            assert_eq!(h.join().unwrap(), 42);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "loader must run exactly once");
    }

    #[test]
    fn gate_is_retired_after_use() {
        let flight: SingleFlight<&'static str> = SingleFlight::new();
        let _ = flight.run::<u64, _, _, ()>(&"k", || None, || Ok(1));
        assert_eq!(flight.lock().len(), 0, "the gate must not leak");
    }

    #[test]
    fn errors_are_not_cached_and_next_caller_retries() {
        let flight: SingleFlight<&'static str> = SingleFlight::new();
        let first = flight.run::<u64, _, _, &'static str>(&"k", || None, || Err("boom"));
        assert_eq!(first, Err("boom"));
        let second = flight.run::<u64, _, _, &'static str>(&"k", || None, || Ok(7));
        assert_eq!(second, Ok(7));
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(f).unwrap()
    }

    #[test]
    fn async_coalesces_across_threads() {
        // 8 threads, each its own executor, all sharing one AsyncSingleFlight.
        // Exercises the cross-thread gate (Arc + waker) and the lost-wakeup-safe
        // registration: exactly one loader runs.
        let store: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let loads = Arc::new(AtomicUsize::new(0));
        let flight: Arc<AsyncSingleFlight<&'static str>> = Arc::new(AsyncSingleFlight::new());
        let barrier = Arc::new(Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (store, loads, flight, barrier) = (
                    Arc::clone(&store),
                    Arc::clone(&loads),
                    Arc::clone(&flight),
                    Arc::clone(&barrier),
                );
                std::thread::spawn(move || {
                    block_on(async move {
                        barrier.wait(); // maximise the race
                        flight
                            .run::<u64, _, _, _, ()>(
                                &"k",
                                || *store.lock().unwrap(),
                                || {
                                    let (store, loads) = (Arc::clone(&store), Arc::clone(&loads));
                                    async move {
                                        loads.fetch_add(1, Ordering::SeqCst);
                                        rt_core::sleep(Duration::from_millis(30)).await;
                                        *store.lock().unwrap() = Some(42);
                                        Ok(42u64)
                                    }
                                },
                            )
                            .await
                            .unwrap()
                    })
                })
            })
            .collect();

        for h in handles {
            assert_eq!(h.join().unwrap(), 42);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "exactly one load across threads");
    }

    #[test]
    fn async_error_not_cached_then_retry() {
        let flight: AsyncSingleFlight<&'static str> = AsyncSingleFlight::new();
        block_on(async {
            let first = flight.run::<u64, _, _, _, &'static str>(&"k", || None, || async { Err("boom") }).await;
            assert_eq!(first, Err("boom"));
            let second = flight.run::<u64, _, _, _, &'static str>(&"k", || None, || async { Ok(7) }).await;
            assert_eq!(second, Ok(7));
        });
    }
}
