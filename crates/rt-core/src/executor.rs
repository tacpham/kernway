//! Executor — the per-shard scheduler.
//!
//! One executor drives one thread. Tasks are never migrated or stolen, so a
//! spawned future does not have to be `Send` and may hold `Rc`, `RefCell`, or
//! any other thread-local state.
//!
//! The loop is: drain woken tasks and poll them; when nothing is runnable, park
//! inside the [`Reactor`] until the OS reports readiness (or another thread
//! unparks us).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::reactor::{Reactor, UNPARK_TOKEN};
use crate::task::{waker_for, Shared, Task, TaskId, MAIN_TASK};
use crate::time::Timers;

thread_local! {
    /// The executor driving this thread, if any — backs [`spawn`] and
    /// [`with_reactor`].
    static CURRENT: RefCell<Option<Rc<Inner>>> = const { RefCell::new(None) };
}

/// Task storage: a slot vector with a free list, so ids are dense and lookup is
/// an index rather than a hash.
#[derive(Default)]
struct Slab {
    slots: Vec<Option<Rc<Task>>>,
    free: Vec<TaskId>,
}

impl Slab {
    fn insert(&mut self, make: impl FnOnce(TaskId) -> Rc<Task>) -> TaskId {
        match self.free.pop() {
            Some(id) => {
                self.slots[id] = Some(make(id));
                id
            }
            None => {
                let id = self.slots.len();
                self.slots.push(Some(make(id)));
                id
            }
        }
    }

    fn get(&self, id: TaskId) -> Option<Rc<Task>> {
        self.slots.get(id).and_then(|s| s.clone())
    }

    fn remove(&mut self, id: TaskId) {
        if let Some(slot) = self.slots.get_mut(id) {
            if slot.take().is_some() {
                self.free.push(id);
            }
        }
    }

    fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }
}

/// Shared guts of an executor, held by both the [`Executor`] and every
/// [`Handle`] handed out from it.
struct Inner {
    tasks: RefCell<Slab>,
    reactor: RefCell<Reactor>,
    timers: RefCell<Timers>,
    shared: Arc<Shared>,
    /// Rounds of a busy run queue since the clock was last read. See
    /// [`ROUNDS_PER_CLOCK_READ`].
    io_tick: Cell<u32>,
    /// When the reactor was last polled, for the [`IO_POLL_INTERVAL`] budget.
    last_io_poll: Cell<Instant>,
}

/// How long a busy run queue may go without the reactor being polled.
///
/// A zero-timeout `poll()` is still a syscall — measured at ~14µs on macOS —
/// so calling it once per round made a self-waking task pay a syscall per
/// yield: `wake_poll_cycle` was 13.5µs where the scheduling work itself is
/// ~47ns. Rationing it is therefore worth a lot.
///
/// The budget is *time*, not a round count, because a round is not a fixed
/// amount of work — "every 61 rounds", the shape tokio's `event_interval`
/// uses, bounds socket latency by a quantity that varies with the workload.
/// A microsecond budget bounds it by the thing actually being promised.
const IO_POLL_INTERVAL: Duration = Duration::from_micros(100);

/// Rounds between clock reads on that path.
///
/// `Instant::now` is ~17ns — cheap next to the syscall it guards, but not next
/// to a 47ns round. Checking every 16th round makes it ~1ns amortized while
/// still resolving the budget far finer than it is wide.
const ROUNDS_PER_CLOCK_READ: u32 = 16;

impl Inner {
    /// Poll one task, removing it from the slab when it completes.
    ///
    /// The slab borrow is released *before* polling: a future is free to spawn
    /// more tasks, which re-enters the slab.
    fn poll_task(&self, id: TaskId) {
        let Some(task) = self.tasks.borrow().get(id) else {
            return; // already finished; a late wake is a no-op
        };
        if task.poll() {
            self.tasks.borrow_mut().remove(id);
        }
    }

    /// Run every task woken so far. Returns how many were polled.
    fn drain_ready(&self, buf: &mut VecDeque<TaskId>) -> usize {
        self.shared.drain_into(buf);
        let mut polled = 0;
        // Tasks woken *during* this drain land in the shared queue and are picked
        // up by the next one, so a self-waking task cannot starve the reactor.
        while let Some(id) = buf.pop_front() {
            self.poll_task(id);
            polled += 1;
        }
        polled
    }

    /// Block until something can make progress.
    ///
    /// `set_parked` is published *before* the "is anything ready?" check, which
    /// is what closes the lost-wakeup race: a wake landing after the check finds
    /// `parked == true` and sends the unparker, and `mio::Waker` also unblocks a
    /// `poll()` that has not started yet.
    fn park(&self) -> io::Result<()> {
        self.shared.set_parked(true);
        if self.shared.has_ready() {
            self.shared.set_parked(false);
            return Ok(());
        }
        // Sleep no longer than the nearest deadline, so timers fire on time
        // without a dedicated thread: the reactor's own timeout is the clock.
        let timeout = self
            .timers
            .borrow_mut()
            .next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()));
        let result = self.reactor.borrow_mut().poll(timeout);
        self.shared.set_parked(false);
        self.mark_io_polled(); // the reactor is current again
        self.fire_timers();
        result.map(|_| ())
    }

    /// Wake every task whose deadline has passed.
    fn fire_timers(&self) {
        // Reading the clock is not free (~17ns, measured) and this runs on every
        // round of a busy run queue. Most shards have no timer pending at any
        // given moment, and for those a heap peek answers the question.
        if self.timers.borrow().is_empty() {
            return;
        }
        let mut expired = Vec::new();
        self.timers
            .borrow_mut()
            .fire_expired(Instant::now(), &mut expired);
        // Wake outside the borrow: a woken task may register a new timer.
        for waker in expired {
            waker.wake();
        }
    }

    /// Service the reactor without blocking.
    ///
    /// The loop must keep doing this while it *doesn't* park. Otherwise a task
    /// that keeps waking itself — a busy poll loop, a chain of `yield_now`s,
    /// any CPU-bound work spread across awaits — keeps the run queue
    /// permanently non-empty, the executor never parks, and no I/O event is
    /// ever collected. Sockets then wait forever on data the kernel already
    /// has. A zero timeout still costs a syscall, so the busy path reaches this
    /// through [`Inner::poll_io_periodic`] rather than calling it every round.
    fn poll_io_now(&self) -> io::Result<()> {
        let result = self
            .reactor
            .borrow_mut()
            .poll(Some(Duration::ZERO))
            .map(|_| ());
        // Timers must advance here too, or a busy run queue would postpone every
        // deadline for as long as it stays busy.
        self.mark_io_polled();
        self.fire_timers();
        result
    }

    /// Restart the I/O budget: the reactor's view of readiness is current.
    fn mark_io_polled(&self) {
        self.io_tick.set(0);
        self.last_io_poll.set(Instant::now());
    }

    /// Service I/O from a *busy* run queue: a real reactor poll once the
    /// [`IO_POLL_INTERVAL`] budget is spent, timers on every round.
    ///
    /// Timers are not rate-limited alongside it because `fire_expired` is a
    /// heap peek rather than a syscall — cheap enough to run every round, and
    /// rationing it would let sleeps overshoot under load.
    fn poll_io_periodic(&self) -> io::Result<()> {
        let tick = self.io_tick.get() + 1;
        if tick >= ROUNDS_PER_CLOCK_READ {
            self.io_tick.set(0);
            if self.last_io_poll.get().elapsed() >= IO_POLL_INTERVAL {
                return self.poll_io_now();
            }
        } else {
            self.io_tick.set(tick);
        }
        self.fire_timers();
        Ok(())
    }
}

/// A cloneable handle to a running executor — spawn from anywhere on its thread.
#[derive(Clone)]
pub struct Handle {
    inner: Rc<Inner>,
}

impl Handle {
    /// Spawn a future onto this executor. The future is polled on this thread
    /// only, so it need not be `Send`.
    pub fn spawn<F>(&self, future: F) -> TaskId
    where
        F: Future<Output = ()> + 'static,
    {
        let shared = Arc::clone(&self.inner.shared);
        let id = self.inner.tasks.borrow_mut().insert(|id| {
            Rc::new(Task::new(Box::pin(future), id, Arc::clone(&shared)))
        });
        // Queue it for the first poll rather than polling inline: spawning from
        // inside a task would otherwise nest polls and re-enter the slab.
        self.inner.shared.schedule(id);
        id
    }

    /// Number of tasks that have not yet completed.
    pub fn task_count(&self) -> usize {
        self.inner.tasks.borrow().len()
    }
}

/// The per-thread scheduler. See the module docs.
pub struct Executor {
    inner: Rc<Inner>,
}

impl Executor {
    /// Create an executor and its reactor.
    pub fn new() -> io::Result<Self> {
        let reactor = Reactor::new()?;
        // The unparker shares the reactor's registry, so a cross-thread wake
        // interrupts exactly the `poll()` this shard sleeps in.
        let unparker = mio::Waker::new(reactor.registry(), UNPARK_TOKEN)?;
        Ok(Self {
            inner: Rc::new(Inner {
                tasks: RefCell::new(Slab::default()),
                reactor: RefCell::new(reactor),
                timers: RefCell::new(Timers::default()),
                shared: Arc::new(Shared::new(unparker)),
                io_tick: Cell::new(0),
                last_io_poll: Cell::new(Instant::now()),
            }),
        })
    }

    /// A handle for spawning onto this executor.
    pub fn handle(&self) -> Handle {
        Handle {
            inner: Rc::clone(&self.inner),
        }
    }

    /// Spawn a future — shorthand for `self.handle().spawn(..)`.
    pub fn spawn<F>(&self, future: F) -> TaskId
    where
        F: Future<Output = ()> + 'static,
    {
        self.handle().spawn(future)
    }

    /// Tasks not yet completed.
    pub fn task_count(&self) -> usize {
        self.inner.tasks.borrow().len()
    }

    /// Install this executor as the thread's current one for the duration of
    /// the guard, so [`spawn`] and [`with_reactor`] work inside futures.
    fn enter(&self) -> EnterGuard {
        CURRENT.with(|c| {
            let mut slot = c.borrow_mut();
            assert!(
                slot.is_none(),
                "another rt-core executor is already running on this thread"
            );
            *slot = Some(Rc::clone(&self.inner));
        });
        EnterGuard
    }

    /// Poll spawned tasks until every one of them has completed.
    ///
    /// Returns as soon as the task set is empty — it does not wait on I/O
    /// sources that no task is watching.
    pub fn run(&self) -> io::Result<()> {
        let _guard = self.enter();
        let mut buf = VecDeque::new();
        loop {
            self.inner.drain_ready(&mut buf);
            if self.inner.tasks.borrow().len() == 0 {
                return Ok(());
            }
            if self.inner.shared.has_ready() {
                self.inner.poll_io_periodic()?; // keep I/O alive under a busy run queue
            } else {
                self.inner.park()?;
            }
        }
    }

    /// Poll everything that is currently runnable and return without parking.
    ///
    /// Useful in tests and for integrating with an outer loop.
    pub fn run_until_stalled(&self) -> usize {
        let _guard = self.enter();
        let mut buf = VecDeque::new();
        let mut total = 0;
        // Collect any readiness that arrived since the last call, so a task
        // parked on a socket becomes runnable in this pass rather than the next.
        let _ = self.inner.poll_io_now();
        loop {
            let polled = self.inner.drain_ready(&mut buf);
            total += polled;
            if polled == 0 {
                return total;
            }
        }
    }

    /// Drive `future` to completion, running spawned tasks alongside it.
    ///
    /// # Hanging
    /// If nothing can ever wake `future` — no spawned task, no registered I/O
    /// source, no waker held elsewhere — this parks indefinitely, exactly like
    /// awaiting a future that is never resolved. It does not try to guess that
    /// case: a waker handed to another thread is indistinguishable from one that
    /// was dropped, and a wrong guess would panic on correct programs.
    pub fn block_on<F: Future>(&self, future: F) -> io::Result<F::Output> {
        let _guard = self.enter();
        let mut future = std::pin::pin!(future);
        let waker = waker_for(MAIN_TASK, Arc::clone(&self.inner.shared));
        let mut cx = Context::from_waker(&waker);
        let mut buf = VecDeque::new();

        loop {
            // Consume any pending wake first: polling clears it, and a wake that
            // arrives during the poll must survive to the next iteration.
            self.inner.shared.take_main_woken();
            if let Poll::Ready(output) = Pin::new(&mut future).poll(&mut cx) {
                return Ok(output);
            }
            self.inner.drain_ready(&mut buf);
            if self.inner.shared.take_main_woken() || self.inner.shared.has_ready() {
                // Progress is possible without sleeping — but still collect I/O
                // periodically, or a self-waking task starves every socket on
                // this shard.
                self.inner.poll_io_periodic()?;
                continue;
            }
            self.inner.park()?;
        }
    }
}

/// Clears the thread's current executor on scope exit.
struct EnterGuard;

impl Drop for EnterGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = None);
    }
}

/// Spawn a future onto the executor running this thread.
///
/// # Panics
/// If called outside [`Executor::run`] / [`Executor::block_on`].
pub fn spawn<F>(future: F) -> TaskId
where
    F: Future<Output = ()> + 'static,
{
    try_handle()
        .expect("rt_core::spawn called outside an executor — wrap the call in Executor::block_on")
        .spawn(future)
}

/// The current thread's executor handle, if one is running.
pub fn try_handle() -> Option<Handle> {
    CURRENT.with(|c| c.borrow().as_ref().map(|inner| Handle { inner: Rc::clone(inner) }))
}

/// Run `f` against the current thread's reactor — the seam `rt-net` registers
/// sockets through.
///
/// # Panics
/// If called outside an executor, or re-entrantly from inside another
/// `with_reactor` closure.
pub fn with_reactor<R>(f: impl FnOnce(&mut Reactor) -> R) -> R {
    try_with_reactor(f).expect("rt_core::with_reactor called outside an executor")
}

/// Run `f` against the current thread's timer heap.
///
/// # Panics
/// If called outside an executor.
pub(crate) fn with_timers<R>(f: impl FnOnce(&mut Timers) -> R) -> R {
    try_with_timers(f).expect("rt_core timers used outside an executor")
}

/// Like [`with_timers`], but `None` when no executor is running — what `Drop`
/// needs, since a timer may outlive the shard that registered it.
pub(crate) fn try_with_timers<R>(f: impl FnOnce(&mut Timers) -> R) -> Option<R> {
    let inner = CURRENT.with(|c| c.borrow().clone())?;
    let mut timers = inner.timers.borrow_mut();
    Some(f(&mut timers))
}

/// Like [`with_reactor`], but returns `None` instead of panicking when no
/// executor is running.
///
/// This is what `Drop` impls want: a socket may well be dropped after its
/// executor has finished, and failing to deregister is harmless there — the
/// reactor it was registered with is already gone.
pub fn try_with_reactor<R>(f: impl FnOnce(&mut Reactor) -> R) -> Option<R> {
    let inner = CURRENT.with(|c| c.borrow().clone())?;
    let mut reactor = inner.reactor.borrow_mut();
    Some(f(&mut reactor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A future that returns `Pending` once, then `Ready` — without any waker
    /// it would hang, so it also proves the wake path works.
    struct YieldOnce(bool);

    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[test]
    fn block_on_returns_the_future_output() {
        let ex = Executor::new().unwrap();
        assert_eq!(ex.block_on(async { 6 * 7 }).unwrap(), 42);
    }

    #[test]
    fn block_on_drives_a_future_that_yields() {
        let ex = Executor::new().unwrap();
        let out = ex
            .block_on(async {
                YieldOnce(false).await;
                YieldOnce(false).await;
                "done"
            })
            .unwrap();
        assert_eq!(out, "done");
    }

    #[test]
    fn run_polls_every_spawned_task_to_completion() {
        let ex = Executor::new().unwrap();
        let counter = Rc::new(Cell::new(0));
        for _ in 0..10 {
            let c = Rc::clone(&counter);
            ex.spawn(async move {
                YieldOnce(false).await;
                c.set(c.get() + 1);
            });
        }
        ex.run().unwrap();
        assert_eq!(counter.get(), 10);
        assert_eq!(ex.task_count(), 0, "finished tasks must free their slots");
    }

    #[test]
    fn spawned_futures_need_not_be_send() {
        // The point of thread-per-core: `Rc` across an await is fine.
        let ex = Executor::new().unwrap();
        let seen = Rc::new(Cell::new(0));
        let local = Rc::clone(&seen);
        ex.spawn(async move {
            let held: Rc<Cell<usize>> = Rc::clone(&local);
            YieldOnce(false).await;
            held.set(1);
        });
        ex.run().unwrap();
        assert_eq!(seen.get(), 1);
    }

    #[test]
    fn a_task_can_spawn_another_task() {
        let ex = Executor::new().unwrap();
        let counter = Rc::new(Cell::new(0));
        let outer = Rc::clone(&counter);
        ex.spawn(async move {
            let inner = Rc::clone(&outer);
            spawn(async move {
                inner.set(inner.get() + 1);
            });
            outer.set(outer.get() + 1);
        });
        ex.run().unwrap();
        assert_eq!(counter.get(), 2, "the child task must run too");
    }

    #[test]
    fn block_on_runs_spawned_tasks_alongside_the_main_future() {
        let ex = Executor::new().unwrap();
        let ticks = Rc::new(Cell::new(0));
        let bg = Rc::clone(&ticks);
        let out = ex
            .block_on(async move {
                spawn(async move {
                    bg.set(bg.get() + 1);
                });
                YieldOnce(false).await; // give the spawned task a chance to run
                YieldOnce(false).await;
                ticks.get()
            })
            .unwrap();
        assert_eq!(out, 1);
    }

    #[test]
    fn slab_slots_are_recycled() {
        let ex = Executor::new().unwrap();
        ex.spawn(async {});
        ex.run().unwrap();
        assert_eq!(ex.task_count(), 0);
        // Second round reuses the freed slot instead of growing the slab.
        let id = ex.spawn(async {});
        assert_eq!(id, 0);
        ex.run().unwrap();
    }

    #[test]
    fn run_until_stalled_does_not_block() {
        let ex = Executor::new().unwrap();
        let done = Rc::new(Cell::new(false));
        let flag = Rc::clone(&done);
        ex.spawn(async move {
            flag.set(true);
        });
        assert!(ex.run_until_stalled() > 0);
        assert!(done.get());
        // Nothing runnable left → returns immediately rather than parking.
        assert_eq!(ex.run_until_stalled(), 0);
    }

    #[test]
    fn a_spinning_task_does_not_starve_a_socket() {
        /// Parks the polling task on `server` becoming readable.
        struct Readable {
            source: Option<mio::net::TcpStream>,
        }

        impl Future for Readable {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                let Some(mut source) = self.source.take() else {
                    return Poll::Ready(()); // woken: the kernel had data for us
                };
                let token = with_reactor(|r| r.register(&mut source).unwrap());
                with_reactor(|r| r.park(token, crate::Direction::Read, cx.waker().clone()));
                std::mem::forget(source); // the test ends here; no deregistration dance
                self.source = None;
                Poll::Pending
            }
        }

        // The reason `poll_io_periodic` may ration the syscall but never drop
        // it: with a task that keeps waking itself the executor never parks, so
        // this is the only path that ever collects readiness. Data already in
        // the kernel must still reach a parked reader.
        use std::io::Write;

        let ex = Executor::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        let server = mio::net::TcpStream::from_std(server);
        client.write_all(b"ping").unwrap();

        // A sibling task spins for far longer than one I/O interval, so the run
        // queue never empties and the executor never parks.
        let spins = Rc::new(Cell::new(0usize));
        // Long enough to outlast several `IO_POLL_INTERVAL` budgets even if a
        // round turns out to be far cheaper than measured.
        const SPIN_ROUNDS: usize = 50_000;
        let counter = Rc::clone(&spins);
        ex.spawn(async move {
            for _ in 0..SPIN_ROUNDS {
                counter.set(counter.get() + 1);
                YieldOnce(false).await;
            }
        });

        let observed = Rc::new(Cell::new(usize::MAX));
        let at_wake = Rc::clone(&observed);
        let seen = Rc::clone(&spins);
        ex.block_on(async move {
            Readable { source: Some(server) }.await;
            at_wake.set(seen.get());
        })
        .unwrap();

        assert!(
            observed.get() < SPIN_ROUNDS,
            "readiness arrived only after the spinner finished ({} of {} rounds) — \
             a busy run queue starved the socket",
            observed.get(),
            SPIN_ROUNDS
        );
    }

    #[test]
    fn a_wake_from_another_thread_unparks_the_executor() {
        // Exercises the whole cross-thread path: Arc waker → shared queue →
        // mio unparker → the parked `poll()` returns.
        struct RemoteWake {
            armed: bool,
            hits: Arc<AtomicUsize>,
        }

        impl Future for RemoteWake {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.armed {
                    return Poll::Ready(());
                }
                self.armed = true;
                let waker = cx.waker().clone();
                let hits = Arc::clone(&self.hits);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(20));
                    hits.fetch_add(1, Ordering::SeqCst);
                    waker.wake();
                });
                Poll::Pending
            }
        }

        let ex = Executor::new().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&hits);
        ex.block_on(RemoteWake { armed: false, hits: counted }).unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn spawn_outside_an_executor_panics_with_a_useful_message() {
        assert!(try_handle().is_none());
        let err = std::panic::catch_unwind(|| spawn(async {})).unwrap_err();
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()).unwrap_or_default());
        assert!(msg.contains("outside an executor"), "got {msg:?}");
    }

    #[test]
    fn current_executor_is_cleared_after_block_on() {
        let ex = Executor::new().unwrap();
        ex.block_on(async {}).unwrap();
        assert!(try_handle().is_none(), "the thread-local must not leak");
    }
}
