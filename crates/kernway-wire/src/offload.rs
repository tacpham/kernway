//! The blocking parts of opening a connection, done off the shard: a driver
//! is fully async, so the system resolver and file reads (which block) run on
//! the blocking pool and are awaited.

use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;

fn panicked(what: &str) -> io::Error {
    io::Error::other(format!("{what}: blocking-pool worker panicked"))
}

/// The address of `host:port`. An IP literal (`[::1]` too) is parsed in
/// place; a name goes to the system resolver on the blocking pool.
pub async fn resolve(host: &str, port: u16) -> io::Result<SocketAddr> {
    let literal = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let name = host.to_string();
    rt_core::spawn_blocking(move || {
        (name.as_str(), port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no address for {name}:{port}")))
    })
    .await
    .ok_or_else(|| panicked("resolve"))?
}

/// Every address of `host:port`, in the resolver's order (a name can map to
/// IPv6 and IPv4 — on Windows `localhost` gives `::1` first — and a server
/// may listen on one family only, so a client tries them in turn).
pub async fn resolve_all(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let literal = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let name = host.to_string();
    rt_core::spawn_blocking(move || {
        let all: Vec<SocketAddr> = (name.as_str(), port).to_socket_addrs()?.collect();
        if all.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("no address for {name}:{port}")));
        }
        Ok(all)
    })
    .await
    .ok_or_else(|| panicked("resolve"))?
}

/// Read a file (a CA bundle, certificate, key) on the blocking pool.
pub async fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    let p = path.to_path_buf();
    rt_core::spawn_blocking(move || std::fs::read(p)).await.ok_or_else(|| panicked("read"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        rt_core::Executor::new().unwrap().block_on(f).unwrap()
    }

    #[test]
    fn ip_literals_are_parsed_and_names_resolved() {
        assert_eq!(run(resolve("127.0.0.1", 5)).unwrap(), "127.0.0.1:5".parse().unwrap());
        assert_eq!(run(resolve("[::1]", 6)).unwrap(), "[::1]:6".parse().unwrap());
        assert!(run(resolve("localhost", 7)).unwrap().ip().is_loopback());
        assert!(run(resolve("kernway-no-such-host.invalid", 8)).is_err());
    }

    #[test]
    fn files_are_read_off_the_shard() {
        let p = std::env::temp_dir().join(format!("kw-offload-{}", std::process::id()));
        std::fs::write(&p, b"pem").unwrap();
        assert_eq!(run(read_file(&p)).unwrap(), b"pem");
        std::fs::remove_file(&p).unwrap();
        assert!(run(read_file(Path::new("/nonexistent/kw.pem"))).is_err());
    }
}
