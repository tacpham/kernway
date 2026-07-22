//! End-to-end tests over real sockets: reactor registration, the readiness
//! loop, and the accept path — the parts unit tests cannot reach without an
//! executor running.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::time::Duration;

use rt_core::Executor;
use rt_net::{AsyncTcpListener, AsyncTcpStream};

/// Start a listener on an ephemeral port inside `ex`, returning its address.
fn bind_local(ex: &Executor) -> (AsyncTcpListener, SocketAddr) {
    ex.block_on(async {
        let listener = AsyncTcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    })
    .unwrap()
}

#[test]
fn echoes_a_payload_back_to_a_blocking_client() {
    let ex = Executor::new().unwrap();
    let (mut listener, addr) = bind_local(&ex);

    let client = std::thread::spawn(move || {
        let mut sock = std::net::TcpStream::connect(addr).unwrap();
        sock.write_all(b"hello kernway").unwrap();
        let mut buf = [0u8; 13];
        sock.read_exact(&mut buf).unwrap();
        buf
    });

    ex.block_on(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.unwrap();
        stream.write_all(&buf[..n]).await.unwrap();
    })
    .unwrap();

    assert_eq!(&client.join().unwrap(), b"hello kernway");
}

#[test]
fn read_returns_zero_when_the_peer_closes() {
    let ex = Executor::new().unwrap();
    let (mut listener, addr) = bind_local(&ex);

    let client = std::thread::spawn(move || {
        let sock = std::net::TcpStream::connect(addr).unwrap();
        drop(sock); // immediate FIN
    });

    ex.block_on(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(stream.read(&mut buf).await.unwrap(), 0, "EOF is Ok(0)");
    })
    .unwrap();
    client.join().unwrap();
}

#[test]
fn async_connect_reaches_an_async_listener() {
    // Both ends on the same shard: connect() must resolve through the writable
    // edge, not block the executor that also has to accept.
    let ex = Executor::new().unwrap();
    let (mut listener, addr) = bind_local(&ex);

    ex.block_on(async move {
        rt_core::spawn(async move {
            let mut client = AsyncTcpStream::connect(addr).await.unwrap();
            client.write_all(b"ping").await.unwrap();
        });

        let (mut server, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4];
        let n = server.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
    })
    .unwrap();
}

#[test]
fn connect_to_a_closed_port_reports_the_error() {
    let ex = Executor::new().unwrap();
    // Bind then drop, so the port is almost certainly free and unlistened.
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };

    let result = ex
        .block_on(async move { AsyncTcpStream::connect(addr).await })
        .unwrap();
    assert!(result.is_err(), "connect to a closed port must fail, not hang");
}

#[test]
fn a_large_payload_survives_short_writes() {
    // 1 MiB will not fit in the socket buffer, so write_all has to loop through
    // several WouldBlock/park cycles.
    const SIZE: usize = 1024 * 1024;
    let ex = Executor::new().unwrap();
    let (mut listener, addr) = bind_local(&ex);

    let client = std::thread::spawn(move || {
        let mut sock = std::net::TcpStream::connect(addr).unwrap();
        let mut got = Vec::with_capacity(SIZE);
        sock.read_to_end(&mut got).unwrap();
        got
    });

    ex.block_on(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let payload: Vec<u8> = (0..SIZE).map(|i| (i % 251) as u8).collect();
        stream.write_all(&payload).await.unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
    })
    .unwrap();

    let got = client.join().unwrap();
    assert_eq!(got.len(), SIZE);
    assert_eq!(got[SIZE - 1], ((SIZE - 1) % 251) as u8);
}

#[test]
fn one_shard_serves_many_connections_concurrently() {
    const CLIENTS: usize = 32;
    let ex = Executor::new().unwrap();
    let (mut listener, addr) = bind_local(&ex);

    let clients = std::thread::spawn(move || {
        let handles: Vec<_> = (0..CLIENTS)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut sock = std::net::TcpStream::connect(addr).unwrap();
                    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let msg = format!("client-{i}");
                    sock.write_all(msg.as_bytes()).unwrap();
                    let mut buf = vec![0u8; msg.len()];
                    sock.read_exact(&mut buf).unwrap();
                    assert_eq!(buf, msg.as_bytes());
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    });

    ex.block_on(async move {
        let served = std::rc::Rc::new(std::cell::Cell::new(0usize));
        for _ in 0..CLIENTS {
            let (mut stream, _) = listener.accept().await.unwrap();
            let served = std::rc::Rc::clone(&served);
            // Each connection is its own task on this same shard. `Rc` across
            // the await is the point: tasks never migrate off this thread.
            rt_core::spawn(async move {
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf).await {
                    let _ = stream.write_all(&buf[..n]).await;
                }
                served.set(served.get() + 1);
            });
        }
        // Let the echo tasks finish before the executor tears their sockets down.
        while served.get() < CLIENTS {
            futures_yield().await;
        }
    })
    .unwrap();

    clients.join().unwrap();
}

#[test]
fn a_busy_task_does_not_starve_socket_io() {
    // Regression: the executor only polled the reactor when it was about to
    // park. A task that keeps waking itself kept the run queue non-empty
    // forever, so readiness events were never collected and every socket on the
    // shard hung — even though the kernel already had the data.
    let ex = Executor::new().unwrap();
    let (mut listener, addr) = bind_local(&ex);

    let client = std::thread::spawn(move || {
        let mut sock = std::net::TcpStream::connect(addr).unwrap();
        // Connect first, send later: the read below must be woken by a readiness
        // event, not by data that happened to be waiting at accept time.
        std::thread::sleep(Duration::from_millis(50));
        sock.write_all(b"wake me").unwrap();
    });

    ex.block_on(async move {
        let spinning = std::rc::Rc::new(std::cell::Cell::new(true));
        let stop = std::rc::Rc::clone(&spinning);
        rt_core::spawn(async move {
            while stop.get() {
                futures_yield().await; // never blocks, always re-queues itself
            }
        });

        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).await.unwrap();
        spinning.set(false);
        assert_eq!(&buf[..n], b"wake me");
    })
    .unwrap();

    client.join().unwrap();
}

/// Hand back to the executor once, so other tasks get polled.
async fn futures_yield() {
    struct YieldNow(bool);
    impl std::future::Future for YieldNow {
        type Output = ();
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            if self.0 {
                std::task::Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }
    }
    YieldNow(false).await
}
