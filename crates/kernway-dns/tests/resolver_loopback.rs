//! Combination test for slices 1 + 2 + 3: the full [`Resolver`] path
//! (config → encode → AsyncUdpSocket → validate id → parse) against a fake
//! nameserver on loopback. No real DNS.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use kernway_dns::message::TYPE_A;
use kernway_dns::{DnsError, Resolver};
use kernway_udp::AsyncUdpSocket;
use rt_core::Executor;

/// Build a reply echoing `id`, with `rcode`, and an optional A record.
fn reply(id: u16, rcode: u8, a: Option<[u8; 4]>) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&id.to_be_bytes());
    let flags = 0x8180u16 | (rcode as u16 & 0x0F); // QR + RD + RA + RCODE
    buf.extend_from_slice(&flags.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    buf.extend_from_slice(&(a.is_some() as u16).to_be_bytes()); // ANCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    let name = |buf: &mut Vec<u8>| {
        for label in ["example", "com"] {
            buf.push(label.len() as u8);
            buf.extend_from_slice(label.as_bytes());
        }
        buf.push(0);
    };
    name(&mut buf); // question
    buf.extend_from_slice(&TYPE_A.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes());
    if let Some(ip) = a {
        name(&mut buf); // answer
        buf.extend_from_slice(&TYPE_A.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&120u32.to_be_bytes());
        buf.extend_from_slice(&4u16.to_be_bytes());
        buf.extend_from_slice(&ip);
    }
    buf
}

/// Spawn a one-shot fake nameserver; returns its address. It reads one query,
/// echoes the client's transaction id, and answers with `rcode`/`a`.
fn spawn_fake_server(rcode: u8, a: Option<[u8; 4]>) -> SocketAddr {
    let server = AsyncUdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = server.local_addr().unwrap();
    rt_core::spawn(async move {
        let mut buf = [0u8; 512];
        let (_n, from) = server.recv_from(&mut buf).await.unwrap();
        let id = u16::from_be_bytes([buf[0], buf[1]]);
        server.send_to(&reply(id, rcode, a), from).await.unwrap();
    });
    addr
}

#[test]
fn resolver_returns_the_a_record() {
    let ex = Executor::new().unwrap();
    let out = ex
        .block_on(async {
            let addr = spawn_fake_server(0, Some([93, 184, 216, 34]));
            let r = Resolver::new(vec![addr]).with_attempts(1);
            r.lookup_a("example.com").await
        })
        .unwrap();
    assert_eq!(out.unwrap(), vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]);
}

#[test]
fn resolver_maps_nxdomain_to_name_not_found() {
    let ex = Executor::new().unwrap();
    let out = ex
        .block_on(async {
            let addr = spawn_fake_server(3, None); // NXDOMAIN
            let r = Resolver::new(vec![addr]).with_attempts(1);
            r.lookup_a("example.com").await
        })
        .unwrap();
    assert_eq!(out, Err(DnsError::NameNotFound));
}

#[test]
fn resolver_times_out_when_the_server_is_silent() {
    let ex = Executor::new().unwrap();
    let out = ex
        .block_on(async {
            // A socket that never replies — the query lands and is ignored.
            // Kept in scope (bound) so the port stays open: a datagram to a
            // *closed* port would draw an ICMP unreachable and fail fast instead
            // of timing out, which is not what we want to exercise.
            let blackhole = AsyncUdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = blackhole.local_addr().unwrap();
            let r = Resolver::new(vec![addr])
                .with_attempts(1)
                .with_timeout(Duration::from_millis(100));
            let out = r.lookup_a("example.com").await;
            drop(blackhole); // explicit: keep it alive until the lookup returns
            out
        })
        .unwrap();
    assert!(matches!(out, Err(DnsError::Io(_))), "expected a timeout I/O error, got {out:?}");
}
