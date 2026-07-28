//! Combination test for slice 6: the per-shard cache short-circuits a repeat
//! lookup. The fake server answers exactly once; the second `lookup_a` must be
//! served from cache — if it reached the (now-silent) network it would time out.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use kernway_dns::message::TYPE_A;
use kernway_dns::Resolver;
use kernway_udp::AsyncUdpSocket;
use rt_core::Executor;

fn reply(id: u16, ip: [u8; 4]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&id.to_be_bytes());
    buf.extend_from_slice(&0x8180u16.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    buf.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    let name = |b: &mut Vec<u8>| {
        for label in ["cached", "example"] {
            b.push(label.len() as u8);
            b.extend_from_slice(label.as_bytes());
        }
        b.push(0);
    };
    name(&mut buf);
    buf.extend_from_slice(&TYPE_A.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes());
    name(&mut buf);
    buf.extend_from_slice(&TYPE_A.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes());
    buf.extend_from_slice(&300u32.to_be_bytes()); // TTL 300 → cache stays valid
    buf.extend_from_slice(&4u16.to_be_bytes());
    buf.extend_from_slice(&ip);
    buf
}

#[test]
fn a_second_lookup_is_served_from_cache_without_touching_the_network() {
    let ex = Executor::new().unwrap();
    let (first, second) = ex
        .block_on(async {
            kernway_dns::cache::clear();
            let server = AsyncUdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = server.local_addr().unwrap();

            // Answer exactly one query, then the task ends (server goes silent).
            rt_core::spawn(async move {
                let mut buf = [0u8; 512];
                let (_n, from) = server.recv_from(&mut buf).await.unwrap();
                let id = u16::from_be_bytes([buf[0], buf[1]]);
                server.send_to(&reply(id, [198, 51, 100, 9]), from).await.unwrap();
            });

            let r = Resolver::new(vec![addr])
                .with_attempts(1)
                .with_timeout(Duration::from_millis(200));

            let first = r.lookup_a("cached.example").await;
            // Server has answered and stopped; only the cache can satisfy this.
            let second = r.lookup_a("cached.example").await;
            (first, second)
        })
        .unwrap();

    let want = vec![IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9))];
    assert_eq!(first.unwrap(), want, "first lookup resolves over the network");
    assert_eq!(second.unwrap(), want, "second lookup is served from cache");
}
