//! Combination test for slices 1 + 2: the async UDP socket
//! (`kernway_udp::AsyncUdpSocket`) carrying an encoded DNS query to a fake
//! server on loopback, and the codec (`kernway_dns::message`) parsing the reply.
//!
//! No real network, no real DNS server — a second socket plays the server and
//! answers with a hand-built packet. This proves encode → send → recv → parse
//! works end to end over the real reactor.

use std::net::{IpAddr, Ipv4Addr};

use kernway_dns::message::{encode_query, parse_response, TYPE_A};
use kernway_udp::AsyncUdpSocket;
use rt_core::Executor;

/// Encode the answer name "example.com", one A record with `ip`, echoing `id`.
fn fake_a_response(id: u16, ip: [u8; 4]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&id.to_be_bytes());
    buf.extend_from_slice(&0x8180u16.to_be_bytes()); // QR + RD + RA, RCODE 0
    buf.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    buf.extend_from_slice(&1u16.to_be_bytes()); // ANCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    // Question: example.com A IN
    let name = |buf: &mut Vec<u8>| {
        for label in ["example", "com"] {
            buf.push(label.len() as u8);
            buf.extend_from_slice(label.as_bytes());
        }
        buf.push(0);
    };
    name(&mut buf);
    buf.extend_from_slice(&TYPE_A.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes());
    // Answer: example.com A IN 300 <ip>
    name(&mut buf);
    buf.extend_from_slice(&TYPE_A.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes());
    buf.extend_from_slice(&300u32.to_be_bytes());
    buf.extend_from_slice(&4u16.to_be_bytes());
    buf.extend_from_slice(&ip);
    buf
}

#[test]
fn encode_send_recv_parse_over_loopback() {
    let ex = Executor::new().unwrap();
    ex.block_on(async {
        let server = AsyncUdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = AsyncUdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();

        // Fake resolver: read one query, echo its id in a canned A response.
        rt_core::spawn(async move {
            let mut buf = [0u8; 512];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert!(n >= 12, "query has at least a header");
            let id = u16::from_be_bytes([buf[0], buf[1]]);
            let resp = fake_a_response(id, [93, 184, 216, 34]);
            server.send_to(&resp, from).await.unwrap();
        });

        // Client: encode → send → recv → parse.
        let query = encode_query(0x4869, "example.com", TYPE_A).unwrap();
        client.send_to(&query, server_addr).await.unwrap();

        let mut buf = [0u8; 512];
        let (n, _) = client.recv_from(&mut buf).await.unwrap();
        let resp = parse_response(&buf[..n]).unwrap();

        assert_eq!(resp.id, 0x4869, "the server echoed our transaction id");
        assert_eq!(resp.rcode, 0);
        assert_eq!(resp.addresses.len(), 1);
        assert_eq!(
            resp.addresses[0].ip,
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))
        );
    })
    .unwrap();
}
