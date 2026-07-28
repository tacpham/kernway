//! Combination test for slice 5: a truncated UDP answer (TC=1) drives the
//! resolver to retry over TCP on the same address. A fake UDP socket and a fake
//! TCP listener share one loopback port; the UDP side always truncates, the TCP
//! side returns the real record.

use std::net::{IpAddr, Ipv4Addr};

use kernway_dns::message::TYPE_A;
use kernway_dns::Resolver;
use kernway_udp::AsyncUdpSocket;
use rt_core::Executor;
use rt_net::AsyncTcpListener;

/// Build a DNS reply echoing `id`; `tc` sets the truncated bit; `a` adds one A.
fn dns_reply(id: u16, tc: bool, a: Option<[u8; 4]>) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&id.to_be_bytes());
    let flags = 0x8180u16 | if tc { 0x0200 } else { 0 }; // QR+RD+RA (+TC)
    buf.extend_from_slice(&flags.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    buf.extend_from_slice(&(a.is_some() as u16).to_be_bytes()); // ANCOUNT
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    let name = |b: &mut Vec<u8>| {
        for label in ["example", "com"] {
            b.push(label.len() as u8);
            b.extend_from_slice(label.as_bytes());
        }
        b.push(0);
    };
    name(&mut buf);
    buf.extend_from_slice(&TYPE_A.to_be_bytes());
    buf.extend_from_slice(&1u16.to_be_bytes());
    if let Some(ip) = a {
        name(&mut buf);
        buf.extend_from_slice(&TYPE_A.to_be_bytes());
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&60u32.to_be_bytes());
        buf.extend_from_slice(&4u16.to_be_bytes());
        buf.extend_from_slice(&ip);
    }
    buf
}

#[test]
fn a_truncated_udp_answer_falls_back_to_tcp() {
    let ex = Executor::new().unwrap();
    let out = ex
        .block_on(async {
            // UDP + TCP on the same loopback port (independent protocol spaces).
            let udp = AsyncUdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let port = udp.local_addr().unwrap().port();
            let mut tcp = AsyncTcpListener::bind(format!("127.0.0.1:{port}").parse().unwrap()).unwrap();

            // UDP: always answer TC=1, no records — forces the fallback.
            rt_core::spawn(async move {
                let mut buf = [0u8; 1500];
                let (_n, from) = udp.recv_from(&mut buf).await.unwrap();
                let id = u16::from_be_bytes([buf[0], buf[1]]);
                udp.send_to(&dns_reply(id, true, None), from).await.unwrap();
            });

            // TCP: read the length-prefixed query, answer with the real A record.
            rt_core::spawn(async move {
                let (mut conn, _peer) = tcp.accept().await.unwrap();
                let mut len_buf = [0u8; 2];
                read_exact(&mut conn, &mut len_buf).await;
                let qlen = u16::from_be_bytes(len_buf) as usize;
                let mut msg = vec![0u8; qlen];
                read_exact(&mut conn, &mut msg).await;
                let id = u16::from_be_bytes([msg[0], msg[1]]);
                let reply = dns_reply(id, false, Some([203, 0, 113, 7]));
                let mut framed = (reply.len() as u16).to_be_bytes().to_vec();
                framed.extend_from_slice(&reply);
                conn.write_all(&framed).await.unwrap();
                let _ = conn.shutdown(std::net::Shutdown::Write);
            });

            let r = Resolver::new(vec![format!("127.0.0.1:{port}").parse().unwrap()]).with_attempts(1);
            r.lookup_a("example.com").await
        })
        .unwrap();

    assert_eq!(out.unwrap(), vec![IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))]);
}

async fn read_exact(conn: &mut rt_net::AsyncTcpStream, buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        let n = conn.read(&mut buf[filled..]).await.unwrap();
        assert!(n > 0, "unexpected EOF in test server");
        filled += n;
    }
}
