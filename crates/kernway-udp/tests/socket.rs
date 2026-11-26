//! `AsyncUdpSocket` over real loopback sockets on the kernway reactor: waking a
//! parked receiver, IPv6, empty and multi-kilobyte datagrams, connected-peer
//! filtering, many datagrams in order, sharing one socket between tasks, and
//! releasing the reactor registration on drop.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use kernway_udp::AsyncUdpSocket;
use rt_core::{sleep, spawn, timeout, Executor};

fn run<F: std::future::Future>(f: F) -> F::Output {
    Executor::new().unwrap().block_on(f).unwrap()
}

fn any(ip: &str) -> SocketAddr {
    format!("{ip}:0").parse().unwrap()
}

/// Bound every wait so a lost wake-up fails the test instead of hanging CI.
const LIMIT: Duration = Duration::from_secs(5);

#[test]
fn a_parked_receiver_wakes_when_a_datagram_arrives_later() {
    run(async {
        let rx = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let to = rx.local_addr().unwrap();
        spawn(async move {
            // Let the receiver park first: nothing is queued yet.
            sleep(Duration::from_millis(50)).await;
            let tx = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
            tx.send_to(b"late", to).await.unwrap();
        });
        let mut buf = [0u8; 8];
        let (n, _) = timeout(LIMIT, rx.recv_from(&mut buf)).await.expect("never woke").unwrap();
        assert_eq!(&buf[..n], b"late");
    });
}

#[test]
fn ipv6_loopback() {
    run(async {
        let a = AsyncUdpSocket::bind(any("[::1]")).unwrap();
        let b = AsyncUdpSocket::bind(any("[::1]")).unwrap();
        assert!(b.local_addr().unwrap().is_ipv6());
        a.send_to(b"v6", b.local_addr().unwrap()).await.unwrap();
        let mut buf = [0u8; 4];
        let (n, from) = timeout(LIMIT, b.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!((&buf[..n], from), (&b"v6"[..], a.local_addr().unwrap()));
    });
}

#[test]
fn empty_and_multi_kilobyte_datagrams_keep_their_boundaries() {
    run(async {
        let a = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let b = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let to = b.local_addr().unwrap();
        // 8000 bytes: under macOS's default 9216-byte datagram cap.
        let big: Vec<u8> = (0..8000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(a.send_to(&[], to).await.unwrap(), 0);
        assert_eq!(a.send_to(&big, to).await.unwrap(), big.len());
        assert_eq!(a.send_to(b"end", to).await.unwrap(), 3);

        let mut buf = vec![0u8; 9000];
        let (n, _) = timeout(LIMIT, b.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(n, 0, "an empty datagram is its own message");
        let (n, _) = timeout(LIMIT, b.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], &big[..]);
        let (n, _) = timeout(LIMIT, b.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"end");
    });
}

#[test]
fn a_connected_socket_only_hears_its_peer() {
    run(async {
        let me = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let peer = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let stranger = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        me.connect(peer.local_addr().unwrap()).unwrap();
        let mine = me.local_addr().unwrap();

        stranger.send_to(b"spam", mine).await.unwrap();
        peer.send_to(b"real", mine).await.unwrap();
        let mut buf = [0u8; 8];
        let n = timeout(LIMIT, me.recv(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"real", "the stranger's datagram was filtered out");

        // And `send` goes to the connected peer.
        me.send(b"back").await.unwrap();
        let (n, from) = timeout(LIMIT, peer.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!((&buf[..n], from), (&b"back"[..], mine));
    });
}

#[test]
fn send_without_a_peer_is_an_error_not_a_hang() {
    run(async {
        let s = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let r = timeout(LIMIT, s.send(b"x")).await.expect("send must not park forever");
        assert!(r.is_err(), "send on an unconnected socket: {r:?}");
    });
}

#[test]
fn many_datagrams_arrive_whole_and_in_order() {
    run(async {
        let a = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let b = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let to = b.local_addr().unwrap();
        let mut buf = [0u8; 64];
        // Batches of 20, drained between batches, so the receive buffer never
        // overflows (loopback UDP drops rather than blocks when it is full).
        for batch in 0..25u32 {
            for i in 0..20u32 {
                a.send_to(&(batch * 20 + i).to_be_bytes(), to).await.unwrap();
            }
            for i in 0..20u32 {
                let (n, _) = timeout(LIMIT, b.recv_from(&mut buf)).await.unwrap().unwrap();
                assert_eq!(n, 4);
                assert_eq!(u32::from_be_bytes(buf[..4].try_into().unwrap()), batch * 20 + i);
            }
        }
    });
}

#[test]
fn one_socket_shared_by_tasks() {
    run(async {
        let shared = Rc::new(AsyncUdpSocket::bind(any("127.0.0.1")).unwrap());
        let echo = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let echo_addr = echo.local_addr().unwrap();
        // An echo task on its own socket.
        spawn(async move {
            let mut buf = [0u8; 16];
            for _ in 0..3 {
                let (n, from) = echo.recv_from(&mut buf).await.unwrap();
                echo.send_to(&buf[..n], from).await.unwrap();
            }
        });
        // Three tasks send through the same socket; the main task collects.
        for word in [&b"one"[..], b"two", b"six"] {
            let s = Rc::clone(&shared);
            spawn(async move {
                s.send_to(word, echo_addr).await.unwrap();
            });
        }
        let got = Rc::new(RefCell::new(Vec::new()));
        let mut buf = [0u8; 16];
        for _ in 0..3 {
            let (n, from) = timeout(LIMIT, shared.recv_from(&mut buf)).await.unwrap().unwrap();
            assert_eq!(from, echo_addr);
            got.borrow_mut().push(buf[..n].to_vec());
        }
        let mut got = got.take();
        got.sort();
        assert_eq!(got, [b"one".to_vec(), b"six".to_vec(), b"two".to_vec()]);
    });
}

#[test]
fn dropping_a_socket_releases_its_port_and_registration() {
    run(async {
        let addr = {
            let s = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
            s.local_addr().unwrap()
        };
        // The same port binds again: the socket was closed on drop.
        let again = AsyncUdpSocket::bind(addr).unwrap();
        assert_eq!(again.local_addr().unwrap(), addr);
        // Many bind/drop cycles on one reactor, then I/O still works.
        for _ in 0..500 {
            drop(AsyncUdpSocket::bind(any("127.0.0.1")).unwrap());
        }
        let tx = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        tx.send_to(b"ok", addr).await.unwrap();
        let mut buf = [0u8; 2];
        let (n, _) = timeout(LIMIT, again.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], b"ok");
        assert!(format!("{again:?}").contains("AsyncUdpSocket"));
    });
}

#[test]
fn binding_an_address_in_use_fails() {
    run(async {
        let a = AsyncUdpSocket::bind(any("127.0.0.1")).unwrap();
        let err = AsyncUdpSocket::bind(a.local_addr().unwrap()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    });
}
