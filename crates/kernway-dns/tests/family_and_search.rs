//! Combination test for slice 7: AAAA lookups, the A→AAAA preference in
//! `lookup`, and search-domain expansion. The fake server parses the query's
//! QNAME and QTYPE so it can answer differently per name / record type.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use kernway_dns::message::{TYPE_A, TYPE_AAAA};
use kernway_dns::Resolver;
use kernway_udp::AsyncUdpSocket;
use rt_core::Executor;

const V6: [u8; 16] = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

/// Extract (qname, qtype) from a raw query (names in a query are literal).
fn question(msg: &[u8]) -> (String, u16) {
    let mut pos = 12;
    let mut labels = Vec::new();
    loop {
        let len = msg[pos] as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        labels.push(String::from_utf8_lossy(&msg[pos..pos + len]).into_owned());
        pos += len;
    }
    let qtype = u16::from_be_bytes([msg[pos], msg[pos + 1]]);
    (labels.join("."), qtype)
}

/// Build a reply echoing `id` for `qtype`, with `rcode` and optional RDATA.
fn reply(id: u16, qtype: u16, rcode: u8, rdata: Option<&[u8]>) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&id.to_be_bytes());
    b.extend_from_slice(&(0x8180u16 | (rcode as u16 & 0x0F)).to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    b.extend_from_slice(&(rdata.is_some() as u16).to_be_bytes()); // ANCOUNT
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    // Question name "x" at offset 12 (a single label), then QTYPE/QCLASS.
    b.push(1);
    b.push(b'x');
    b.push(0);
    b.extend_from_slice(&qtype.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    if let Some(rd) = rdata {
        b.push(0xC0); // compression pointer to the question name at offset 12
        b.push(12);
        b.extend_from_slice(&qtype.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&60u32.to_be_bytes());
        b.extend_from_slice(&(rd.len() as u16).to_be_bytes());
        b.extend_from_slice(rd);
    }
    b
}

#[test]
fn lookup_aaaa_returns_ipv6() {
    let ex = Executor::new().unwrap();
    let out = ex
        .block_on(async {
            kernway_dns::cache::clear();
            let server = AsyncUdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = server.local_addr().unwrap();
            rt_core::spawn(async move {
                let mut buf = [0u8; 1500];
                let (_n, from) = server.recv_from(&mut buf).await.unwrap();
                let id = u16::from_be_bytes([buf[0], buf[1]]);
                server.send_to(&reply(id, TYPE_AAAA, 0, Some(&V6)), from).await.unwrap();
            });
            let r = Resolver::new(vec![addr]).with_attempts(1);
            r.lookup_aaaa("v6.example").await
        })
        .unwrap();
    assert_eq!(out.unwrap(), vec![IpAddr::V6(Ipv6Addr::from(V6))]);
}

#[test]
fn lookup_prefers_a_but_falls_back_to_aaaa_when_there_is_no_a() {
    let ex = Executor::new().unwrap();
    let out = ex
        .block_on(async {
            kernway_dns::cache::clear();
            let server = AsyncUdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = server.local_addr().unwrap();
            // A → NOERROR with no records; AAAA → the v6 address.
            rt_core::spawn(async move {
                let mut buf = [0u8; 1500];
                for _ in 0..4 {
                    let (_n, from) = server.recv_from(&mut buf).await.unwrap();
                    let id = u16::from_be_bytes([buf[0], buf[1]]);
                    let (_name, qtype) = question(&buf);
                    let msg = if qtype == TYPE_AAAA {
                        reply(id, TYPE_AAAA, 0, Some(&V6))
                    } else {
                        reply(id, TYPE_A, 0, None)
                    };
                    server.send_to(&msg, from).await.unwrap();
                }
            });
            let r = Resolver::new(vec![addr]).with_attempts(1);
            r.lookup("dualstack.example").await
        })
        .unwrap();
    assert_eq!(out.unwrap(), vec![IpAddr::V6(Ipv6Addr::from(V6))]);
}

#[test]
fn a_search_domain_suffix_resolves_a_bare_name() {
    let ex = Executor::new().unwrap();
    let out = ex
        .block_on(async {
            kernway_dns::cache::clear();
            let server = AsyncUdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = server.local_addr().unwrap();
            // Only the suffixed name exists; the bare name would be NXDOMAIN.
            rt_core::spawn(async move {
                let mut buf = [0u8; 1500];
                for _ in 0..4 {
                    let (_n, from) = server.recv_from(&mut buf).await.unwrap();
                    let id = u16::from_be_bytes([buf[0], buf[1]]);
                    let (name, qtype) = question(&buf);
                    let msg = if name == "web.corp.example" {
                        reply(id, qtype, 0, Some(&[10, 1, 2, 3]))
                    } else {
                        reply(id, qtype, 3, None) // NXDOMAIN
                    };
                    server.send_to(&msg, from).await.unwrap();
                }
            });
            let r = Resolver::new(vec![addr])
                .with_attempts(1)
                .with_search(vec!["corp.example".into()], 1);
            r.lookup_a("web").await
        })
        .unwrap();
    assert_eq!(out.unwrap(), vec![IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))]);
}
