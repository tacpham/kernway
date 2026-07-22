//! Shard bootstrap — one listener, one executor, one thread per core.

use std::future::Future;
use std::io;
use std::net::{SocketAddr, TcpListener};

use rt_core::Executor;

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
}

impl ShardConfig {
    /// Config for `addr` with one shard per CPU.
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            shards: rt_core::default_shard_count(),
            backlog: 1024,
            pin_threads: true,
            nodelay: true,
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
}

/// Create one listener per shard, all bound to the same address.
///
/// On Linux each gets its own `SO_REUSEPORT` socket, so the kernel hashes
/// incoming connections across them and no shared accept queue exists. Where
/// that is unavailable ([`sys::supports_reuseport`] is false) this returns a
/// **single** listener — the caller must not assume `len() == config.shards`.
pub fn bootstrap_shards(config: &ShardConfig) -> io::Result<Vec<TcpListener>> {
    if !sys::supports_reuseport() {
        return Ok(vec![sys::bind_listener(config.addr, config.backlog, false)?]);
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
/// There is no graceful-shutdown path yet — that lands with the drain timeout in
/// v0.3. Today the process ends the server.
pub fn run_shards<F, Fut>(config: ShardConfig, handler: F) -> io::Result<()>
where
    F: Fn(AsyncTcpStream) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let listeners = bootstrap_shards(&config)?;
    let mut threads = Vec::with_capacity(listeners.len());

    for (index, listener) in listeners.into_iter().enumerate() {
        let handler = handler.clone();
        let config = config.clone();
        threads.push(
            std::thread::Builder::new()
                .name(format!("kernway-shard-{index}"))
                .spawn(move || shard_main(index, listener, config, handler))?,
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

/// The body of one shard thread: pin, build an executor, accept forever.
fn shard_main<F, Fut>(
    index: usize,
    listener: TcpListener,
    config: ShardConfig,
    handler: F,
) -> io::Result<()>
where
    F: Fn(AsyncTcpStream) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    if config.pin_threads {
        // Advisory: unpinned shards are correct, just less cache-friendly, so a
        // platform that cannot pin must not fail to start.
        if let Err(e) = rt_core::pin_current_thread_to_core(index) {
            eprintln!("kernway: shard {index} runs unpinned ({e})");
        }
    }

    let executor = Executor::new()?;
    executor.block_on(async move {
        let mut listener = AsyncTcpListener::from_std(listener)?;
        loop {
            let (stream, _peer) = listener.accept().await?;
            if config.nodelay {
                let _ = stream.set_nodelay(true);
            }
            // One task per connection, on this shard — never migrated.
            rt_core::spawn(handler(stream));
        }
    })?
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
}
