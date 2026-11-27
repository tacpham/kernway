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

/// Connect to `host:port`, trying every address the name resolves to in
/// order until one accepts. A name often maps to both `::1` and
/// `127.0.0.1` (`localhost`; Windows lists IPv6 first) while a server
/// listens on one family only — taking just the first address would fail
/// there. When every address refuses, the error names each attempt.
pub async fn connect_tcp(host: &str, port: u16) -> io::Result<rt_net::AsyncTcpStream> {
    // A name that does not resolve is `NotFound`, whatever the OS reports,
    // so callers can tell it from a refused connection.
    let addrs = resolve_all(host, port)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::NotFound, format!("resolve {host}:{port}: {e}")))?;
    let mut failures = Vec::new();
    let mut kind = io::ErrorKind::ConnectionRefused;
    for addr in &addrs {
        match rt_net::AsyncTcpStream::connect(*addr).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                kind = e.kind();
                failures.push(format!("{addr}: {e}"));
            }
        }
    }
    Err(io::Error::new(kind, failures.join("; ")))
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
    fn connect_tcp_tries_every_address_of_a_name() {
        // A server on IPv4 loopback only, reached by the name `localhost`
        // (which resolves to ::1 first on macOS and Windows).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = std::thread::spawn(move || listener.accept().map(|_| ()));
        let s = run(connect_tcp("localhost", port)).expect("connects over IPv4 after ::1 refuses");
        assert_eq!(s.peer_addr().unwrap(), format!("127.0.0.1:{port}").parse().unwrap());
        accepted.join().unwrap().unwrap();
        // Nothing listening: the error names every address tried.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let e = run(connect_tcp("localhost", closed)).err().unwrap();
        assert!(e.to_string().contains(&format!("127.0.0.1:{closed}")), "{e}");
        let e = run(connect_tcp("kernway-no-such-host.invalid", 1)).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }

    #[test]
    fn resolve_all_gives_every_address_and_parses_literals_in_place() {
        let all = run(resolve_all("localhost", 6379)).unwrap();
        assert!(!all.is_empty());
        assert!(all.iter().all(|a| a.ip().is_loopback() && a.port() == 6379), "{all:?}");
        assert_eq!(run(resolve_all("[::1]", 1)).unwrap(), vec!["[::1]:1".parse().unwrap()]);
        assert_eq!(run(resolve_all("10.0.0.1", 2)).unwrap(), vec!["10.0.0.1:2".parse().unwrap()]);
        assert!(run(resolve_all("kernway-no-such-host.invalid", 3)).is_err());
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
