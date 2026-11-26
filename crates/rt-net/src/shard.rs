//! Shard bootstrap — one listener, one executor, one thread per core.

use std::cell::Cell;
use std::future::Future;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use rt_core::{Executor, Shutdown};

use crate::listener::AsyncTcpListener;
use crate::stream::AsyncTcpStream;
use crate::sys;

/// How to spread a server across cores.
#[derive(Debug, Clone)]
pub struct ShardConfig {
    /// Address every shard binds.
    pub addr: SocketAddr,
    /// Number of shards. Defaults to one per CPU.
    pub shards: usize,
    /// `listen(2)` backlog per shard.
    pub backlog: i32,
    /// Pin each shard's thread to a core. Advisory — see
    /// [`rt_core::pin_current_thread_to_core`].
    pub pin_threads: bool,
    /// Disable Nagle on accepted connections.
    pub nodelay: bool,
    /// How long a shard waits for in-flight connections after it stops
    /// accepting. See [`run_shards_with_shutdown`].
    pub drain_timeout: Duration,
}

/// Default grace period for in-flight connections.
///
/// Long enough for a request that is already being served to finish, short
/// enough to stay inside the termination grace period orchestrators give a
/// container by default (Kubernetes: 30s, then `SIGKILL`) — a drain that
/// outlives that window is not a graceful shutdown, it is a kill with extra
/// steps.
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

/// How often a draining shard re-checks whether its connections are done.
///
/// The check is a task-count read, not a syscall, and it only runs while the
/// shard is shutting down — so this trades a few idle wakeups during shutdown
/// for not having to plumb a completion signal through every connection task.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(1);

impl ShardConfig {
    /// Config for `addr` with one shard per CPU.
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            shards: rt_core::default_shard_count(),
            backlog: 1024,
            pin_threads: true,
            nodelay: true,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
        }
    }

    /// Override the shard count.
    pub fn shards(mut self, shards: usize) -> Self {
        self.shards = shards.max(1);
        self
    }

    /// Override the listen backlog.
    pub fn backlog(mut self, backlog: i32) -> Self {
        self.backlog = backlog;
        self
    }

    /// Turn thread pinning on or off.
    pub fn pin_threads(mut self, pin: bool) -> Self {
        self.pin_threads = pin;
        self
    }

    /// How long to let in-flight connections finish after shutdown is signalled.
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }
}

thread_local! {
    /// This thread's shard index, set once when a shard thread starts.
    static SHARD_INDEX: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The index of the shard running on this thread, or `None` off-shard (the
/// main thread, a `spawn_blocking` worker, a test).
///
/// Indexes are `0..shard_thread_count(&config)`, the same numbering
/// [`run_shards_with_init`]'s `init` receives — so it doubles as "which
/// [`CoreMesh`](crate::CoreMesh) receiver is mine".
pub fn shard_index() -> Option<usize> {
    SHARD_INDEX.with(Cell::get)
}

/// How many shard threads [`run_shards`] and friends will start for `config`.
///
/// Usually `config.shards`, but where `SO_REUSEPORT` is unavailable
/// ([`sys::supports_reuseport`] is false) there is only one listener and
/// therefore one shard. Size anything indexed by shard — a
/// [`CoreMesh`](crate::CoreMesh) in particular — with this, not with
/// `config.shards`, or messages routed to a shard that never started are never
/// received.
pub fn shard_thread_count(config: &ShardConfig) -> usize {
    if sys::supports_reuseport() {
        config.shards
    } else {
        1
    }
}

/// Create one listener per shard, all bound to the same address.
///
/// On Linux each gets its own `SO_REUSEPORT` socket, so the kernel hashes
/// incoming connections across them and no shared accept queue exists. Where
/// that is unavailable ([`sys::supports_reuseport`] is false) this returns a
/// **single** listener — the caller must not assume `len() == config.shards`.
pub fn bootstrap_shards(config: &ShardConfig) -> io::Result<Vec<TcpListener>> {
    if !sys::supports_reuseport() {
        return Ok(vec![sys::bind_listener(
            config.addr,
            config.backlog,
            false,
        )?]);
    }

    let mut listeners = Vec::with_capacity(config.shards);
    // Bind the first socket explicitly so port 0 resolves to a concrete port
    // that the remaining shards can share.
    let first = sys::bind_listener(config.addr, config.backlog, true)?;
    let addr = first.local_addr()?;
    listeners.push(first);
    for _ in 1..config.shards {
        listeners.push(sys::bind_listener(addr, config.backlog, true)?);
    }
    Ok(listeners)
}

/// Run `handler` for every accepted connection, across all shards. Blocks until
/// every shard thread exits (i.e. normally forever).
///
/// `handler` is cloned into each shard thread; the futures it returns are polled
/// only on the shard that accepted the connection, so they need not be `Send`.
///
/// # Shutdown
/// This form never returns on its own — only the process ends it. Use
/// [`run_shards_with_shutdown`] to stop on a signal and drain in-flight
/// connections first.
pub fn run_shards<F, Fut>(config: ShardConfig, handler: F) -> io::Result<()>
where
    F: Fn(AsyncTcpStream) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    // A signal nobody holds a trigger for can never fire, so the accept loop
    // below is exactly the unconditional one, with no extra poll on the path.
    run_shards_with_shutdown(config, Shutdown::new(), handler)
}

/// Like [`run_shards`], but every shard stops when `shutdown` fires.
///
/// Shutdown happens in two steps, and the order is what makes it graceful:
///
/// 1. **Stop accepting.** Each shard drops its listener the moment the signal
///    arrives, which closes the socket and releases the port. Connections the
///    kernel had already queued but nobody accepted are reset — the client sees
///    a refused connection and retries, rather than a request that is accepted
///    and then abandoned half-served.
/// 2. **Drain.** Connections already accepted keep running for up to
///    [`ShardConfig::drain_timeout`]. A shard that reaches the deadline with
///    work outstanding reports how much it abandoned on stderr and exits
///    anyway; the alternative — waiting on a client that never sends its next
///    byte — is a shutdown that never completes.
///
/// Returns once every shard has finished draining.
///
/// ```no_run
/// use rt_core::Shutdown;
/// use rt_net::{run_shards_with_shutdown, ShardConfig};
///
/// let shutdown = Shutdown::new();
/// rt_core::on_interrupt({
///     let shutdown = shutdown.clone();
///     move || shutdown.trigger()
/// })
/// .unwrap();
///
/// let config = ShardConfig::new("0.0.0.0:8080".parse().unwrap());
/// run_shards_with_shutdown(config, shutdown, |_stream| async {}).unwrap();
/// ```
pub fn run_shards_with_shutdown<F, Fut>(
    config: ShardConfig,
    shutdown: Shutdown,
    handler: F,
) -> io::Result<()>
where
    F: Fn(AsyncTcpStream) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    run_shards_with_init(config, shutdown, |_index| {}, handler)
}

/// Like [`run_shards_with_shutdown`], plus a per-shard `init` hook.
///
/// `init(index)` runs once on each shard thread, inside that shard's executor
/// and before its first `accept`, so it can [`rt_core::spawn`] shard-local
/// background tasks — the consumer of a [`CoreMesh`](crate::CoreMesh)
/// receiver, a per-core cache sweeper. `index` matches [`shard_index`].
///
/// Tasks spawned here count toward the drain: a task that never finishes holds
/// its shard open for the full [`ShardConfig::drain_timeout`]. Have it stop on
/// `shutdown` too (e.g. wrap its receive in [`rt_core::until_shutdown`]).
///
/// ```no_run
/// use std::sync::{Arc, Mutex};
/// use rt_core::Shutdown;
/// use rt_net::{run_shards_with_init, shard_thread_count, CoreMesh, ShardConfig};
///
/// let config = ShardConfig::new("0.0.0.0:8080".parse().unwrap());
/// let (mesh, receivers) = CoreMesh::<String>::new(shard_thread_count(&config), 1024);
/// // Each shard takes its own receiver out of the table at startup.
/// let slots = Arc::new(Mutex::new(receivers.into_iter().map(Some).collect::<Vec<_>>()));
/// let shutdown = Shutdown::new();
///
/// let init = {
///     let shutdown = shutdown.clone();
///     move |index: usize| {
///         let mut rx = slots.lock().unwrap()[index].take().expect("one receiver per shard");
///         let shutdown = shutdown.clone();
///         rt_core::spawn(async move {
///             while let Some(batch) = rt_core::until_shutdown(&shutdown, rx.recv_batch(64)).await {
///                 if batch.is_empty() {
///                     break; // every sender is gone
///                 }
///                 for msg in batch {
///                     let _ = msg; // handle it on this shard
///                 }
///             }
///         });
///     }
/// };
///
/// run_shards_with_init(config, shutdown, init, move |_stream| {
///     let mesh = mesh.clone();
///     async move {
///         let room = mesh.core_for(42);
///         let _ = mesh.send_to(room, "join".to_string());
///     }
/// })
/// .unwrap();
/// ```
pub fn run_shards_with_init<I, F, Fut>(
    config: ShardConfig,
    shutdown: Shutdown,
    init: I,
    handler: F,
) -> io::Result<()>
where
    I: Fn(usize) + Clone + Send + 'static,
    F: Fn(AsyncTcpStream) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let listeners = bootstrap_shards(&config)?;
    let mut threads = Vec::with_capacity(listeners.len());

    for (index, listener) in listeners.into_iter().enumerate() {
        let handler = handler.clone();
        let init = init.clone();
        let config = config.clone();
        let shutdown = shutdown.clone();
        threads.push(
            std::thread::Builder::new()
                .name(format!("kernway-shard-{index}"))
                .spawn(move || shard_main(index, listener, config, shutdown, init, handler))?,
        );
    }

    for thread in threads {
        match thread.join() {
            Ok(result) => result?,
            Err(_) => return Err(io::Error::other("a shard thread panicked")),
        }
    }
    Ok(())
}

/// The body of one shard thread: pin, build an executor, accept until told to
/// stop, then drain.
fn shard_main<I, F, Fut>(
    index: usize,
    listener: TcpListener,
    config: ShardConfig,
    shutdown: Shutdown,
    init: I,
    handler: F,
) -> io::Result<()>
where
    I: Fn(usize),
    F: Fn(AsyncTcpStream) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    SHARD_INDEX.with(|slot| slot.set(Some(index)));
    if config.pin_threads {
        // Advisory: unpinned shards are correct, just less cache-friendly, so a
        // platform that cannot pin must not fail to start.
        if let Err(e) = rt_core::pin_current_thread_to_core(index) {
            // Expected on macOS/Windows (no affinity API) — advisory, not a problem.
            // Diagnostic, so debug: quiet in production, visible when you ask for it.
            kernway_log::debug!(target: "rt_net", "shard {index} runs unpinned: {e}");
        }
    }

    let executor = Executor::new()?;
    executor.block_on(async move {
        let mut listener = AsyncTcpListener::from_std(listener)?;
        // After the listener is registered (a bind failure surfaces first) and
        // before the first accept, so shard-local tasks exist by the time the
        // first connection could need them.
        init(index);
        while let Some(accepted) = rt_core::until_shutdown(&shutdown, listener.accept()).await {
            let (stream, _peer) = accepted?;
            if config.nodelay {
                let _ = stream.set_nodelay(true);
            }
            // One task per connection, on this shard — never migrated.
            rt_core::spawn(handler(stream));
        }
        // Close the listening socket before draining: a client that connects
        // now must be refused outright, not accepted by a server on its way out.
        drop(listener);

        let abandoned = drain(config.drain_timeout).await;
        if abandoned > 0 {
            kernway_log::warn!(
                target: "rt_net",
                "shard {index} gave up on {abandoned} connection(s) after {:?} of draining",
                config.drain_timeout
            );
        }
        Ok(())
    })?
}

/// Wait for this shard's connection tasks to finish, up to `timeout`.
///
/// Returns how many were still running when the deadline passed — zero on a
/// clean drain.
///
/// Polling the task count is deliberate. The alternative, a completion signal
/// per connection, would put a counter update on the end of every request just
/// to serve a path taken once in a process's lifetime; this pays nothing while
/// the server is running and a 1ms tick only while it is stopping.
async fn drain(timeout: Duration) -> usize {
    let handle = rt_core::try_handle().expect("drain runs inside the shard's executor");
    let deadline = Instant::now() + timeout;
    loop {
        let outstanding = handle.task_count();
        if outstanding == 0 {
            return 0;
        }
        if Instant::now() >= deadline {
            return outstanding;
        }
        rt_core::sleep(DRAIN_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_one_shard_per_cpu() {
        let config = ShardConfig::new("127.0.0.1:0".parse().unwrap());
        assert_eq!(config.shards, rt_core::default_shard_count());
        assert!(config.shards >= 1);
    }

    #[test]
    fn shard_count_is_clamped_to_at_least_one() {
        let config = ShardConfig::new("127.0.0.1:0".parse().unwrap()).shards(0);
        assert_eq!(config.shards, 1, "zero shards would accept nothing");
    }

    #[test]
    fn bootstrap_binds_every_shard_to_the_same_port() {
        let config = ShardConfig::new("127.0.0.1:0".parse().unwrap()).shards(4);
        let listeners = bootstrap_shards(&config).unwrap();

        if sys::supports_reuseport() {
            assert_eq!(listeners.len(), 4);
        } else {
            assert_eq!(listeners.len(), 1, "no SO_REUSEPORT → a single listener");
        }

        let port = listeners[0].local_addr().unwrap().port();
        assert_ne!(port, 0);
        for listener in &listeners {
            assert_eq!(listener.local_addr().unwrap().port(), port);
        }
    }

    #[test]
    fn bootstrapped_listeners_accept_real_connections() {
        let config = ShardConfig::new("127.0.0.1:0".parse().unwrap()).shards(2);
        let listeners = bootstrap_shards(&config).unwrap();
        let addr = listeners[0].local_addr().unwrap();

        // Which shard the kernel picks is its business (and on macOS it does not
        // balance at all) — the guarantee under test is only that a connection
        // to the shared port lands on one of them.
        let client = std::thread::spawn(move || std::net::TcpStream::connect(addr));
        let mut accepted = false;
        for listener in &listeners {
            listener.set_nonblocking(true).unwrap();
        }
        for _ in 0..200 {
            for listener in &listeners {
                if listener.accept().is_ok() {
                    accepted = true;
                }
            }
            if accepted {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        client.join().unwrap().unwrap();
        assert!(accepted, "no shard accepted the connection");
    }

    /// A server on an ephemeral port, plus the address it ended up on.
    ///
    /// The port has to be read back from the bound listener *before* the shards
    /// take it, so the test can connect without guessing.
    fn spawn_server<F, Fut>(
        shutdown: Shutdown,
        drain_timeout: Duration,
        handler: F,
    ) -> (SocketAddr, std::thread::JoinHandle<io::Result<()>>)
    where
        F: Fn(AsyncTcpStream) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        // One shard: the assertions are about the shutdown path, and a single
        // listener makes "the port is closed" unambiguous.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let config = ShardConfig::new(addr)
            .shards(1)
            .pin_threads(false)
            .drain_timeout(drain_timeout);
        let server =
            std::thread::spawn(move || run_shards_with_shutdown(config, shutdown, handler));

        // Wait for the bind to land before handing the address back.
        for _ in 0..500 {
            if std::net::TcpStream::connect(addr).is_ok() {
                return (addr, server);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("server never came up on {addr}");
    }

    #[test]
    fn init_runs_once_per_shard_with_its_index_and_can_consume_a_mesh() {
        use crate::CoreMesh;
        use std::sync::{mpsc, Arc, Mutex};

        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let config = ShardConfig::new(addr).shards(2).pin_threads(false);
        let shards = shard_thread_count(&config);

        let (mesh, receivers) = CoreMesh::<usize>::new(shards, 16);
        let slots = Arc::new(Mutex::new(
            receivers.into_iter().map(Some).collect::<Vec<_>>(),
        ));
        // (shard that received, message) for every delivered message.
        let (seen_tx, seen_rx) = mpsc::channel::<(usize, usize)>();
        let (started_tx, started_rx) = mpsc::channel::<(usize, Option<usize>)>();
        let shutdown = Shutdown::new();

        let init = {
            let shutdown = shutdown.clone();
            move |index: usize| {
                let _ = started_tx.send((index, shard_index()));
                let mut rx = slots.lock().unwrap()[index]
                    .take()
                    .expect("init must run once per shard");
                let shutdown = shutdown.clone();
                let seen = seen_tx.clone();
                rt_core::spawn(async move {
                    while let Some(Some(msg)) = rt_core::until_shutdown(&shutdown, rx.recv()).await
                    {
                        let _ = seen.send((index, msg));
                    }
                });
            }
        };
        let server = {
            let shutdown = shutdown.clone();
            std::thread::spawn(move || {
                run_shards_with_init(config, shutdown, init, |_stream| async {})
            })
        };

        let mut started: Vec<_> = (0..shards)
            .map(|_| started_rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        started.sort();
        for (index, (got, from_thread_local)) in started.into_iter().enumerate() {
            assert_eq!(got, index);
            assert_eq!(
                from_thread_local,
                Some(index),
                "shard_index() disagrees with init"
            );
        }
        assert_eq!(shard_index(), None, "the test thread is not a shard");

        // Sent from this (non-shard) thread; each must surface on its own shard.
        for core in 0..shards {
            mesh.send_to(core, 100 + core).unwrap();
        }
        let mut seen: Vec<_> = (0..shards)
            .map(|_| seen_rx.recv_timeout(Duration::from_secs(10)).unwrap())
            .collect();
        seen.sort();
        let expected: Vec<_> = (0..shards).map(|core| (core, 100 + core)).collect();
        assert_eq!(seen, expected);

        shutdown.trigger();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn a_triggered_shutdown_stops_every_shard() {
        let shutdown = Shutdown::new();
        let (_addr, server) =
            spawn_server(shutdown.clone(), Duration::from_secs(5), |_stream| async {});

        shutdown.trigger();
        // `run_shards_with_shutdown` joins its threads, so returning at all
        // means every shard left its accept loop.
        let started = Instant::now();
        server.join().unwrap().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the shard sat in accept() instead of observing the signal"
        );
    }

    #[test]
    fn the_port_is_released_once_the_server_stops() {
        let shutdown = Shutdown::new();
        let (addr, server) =
            spawn_server(shutdown.clone(), Duration::from_secs(5), |_stream| async {});

        shutdown.trigger();
        server.join().unwrap().unwrap();

        // Nothing is listening any more: a fresh bind must succeed, and it is
        // the same check a rolling restart makes when the next process starts.
        std::net::TcpListener::bind(addr).expect("the listening socket outlived the shutdown");
    }

    #[test]
    fn an_in_flight_connection_finishes_before_the_shard_exits() {
        use std::io::Read;

        let shutdown = Shutdown::new();
        // The handler is still working when the signal arrives; a shutdown that
        // dropped it here would cut a half-written response.
        let (addr, server) = spawn_server(
            shutdown.clone(),
            Duration::from_secs(5),
            |mut stream| async move {
                rt_core::sleep(Duration::from_millis(150)).await;
                let _ = stream.write_all(b"done").await;
            },
        );

        let mut client = std::net::TcpStream::connect(addr).unwrap();
        // Give the shard a moment to accept, so the connection is genuinely
        // in-flight rather than still queued in the kernel.
        std::thread::sleep(Duration::from_millis(30));
        shutdown.trigger();

        let mut got = String::new();
        client.read_to_string(&mut got).unwrap();
        assert_eq!(got, "done", "the drain cut an in-flight connection short");
        server.join().unwrap().unwrap();
    }

    #[test]
    fn the_drain_timeout_bounds_a_connection_that_never_ends() {
        let shutdown = Shutdown::new();
        let (addr, server) = spawn_server(
            shutdown.clone(),
            Duration::from_millis(100),
            |_stream| async {
                // A client holding a connection open forever must not be able to
                // hold the shutdown open forever with it.
                std::future::pending::<()>().await;
            },
        );

        let _client = std::net::TcpStream::connect(addr).unwrap();
        std::thread::sleep(Duration::from_millis(30));

        let started = Instant::now();
        shutdown.trigger();
        server.join().unwrap().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a stuck connection blocked the shutdown: {:?}",
            started.elapsed()
        );
    }
}
