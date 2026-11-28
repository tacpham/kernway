//! The client against servers that misbehave on purpose: not SSH at all, an old
//! protocol version, a key exchange that cannot agree, a server that hangs up in
//! the middle, a man in the middle who swaps the host key. Each is a few lines of
//! loopback TCP — what no well-behaved server sends on demand.

use kernway_ssh::{Auth, HostKeyPolicy, PrivateKey, SshClient, SshConfig, SshError};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

/// A one-connection server that writes `greeting`, then reads and drops input until
/// the client hangs up (or closes at once when `hang_up`).
fn server(greeting: Vec<u8>, hang_up: bool) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let _ = s.write_all(&greeting);
        if hang_up {
            return;
        }
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut b = [0u8; 4096];
        while matches!(s.read(&mut b), Ok(n) if n > 0) {}
    });
    port
}

fn key() -> PrivateKey {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", kernway_wire::base64::encode(pkcs8.as_ref()));
    PrivateKey::from_pem(pem.as_bytes()).unwrap()
}

fn connect(port: u16) -> SshError {
    let cfg = SshConfig::new("127.0.0.1", "kw").port(port).auth(Auth::Key(key())).host_key(HostKeyPolicy::AcceptAny).connect_timeout(Duration::from_secs(3));
    rt_core::Executor::new().unwrap().block_on(SshClient::connect(cfg)).unwrap().unwrap_err()
}

/// A KEXINIT packet (unencrypted framing) offering these algorithms.
fn kexinit(kex: &str, host_key: &str, cipher: &str, follows: bool) -> Vec<u8> {
    let mut p = vec![20u8];
    p.extend([0u8; 16]);
    for list in [kex, host_key, cipher, cipher, "hmac-sha2-256", "hmac-sha2-256", "none", "none", "", ""] {
        p.extend((list.len() as u32).to_be_bytes());
        p.extend(list.as_bytes());
    }
    p.push(u8::from(follows));
    p.extend([0u8; 4]);
    let mut pad = 8 - (5 + p.len()) % 8;
    if pad < 4 {
        pad += 8;
    }
    let mut out = ((1 + p.len() + pad) as u32).to_be_bytes().to_vec();
    out.push(pad as u8);
    out.extend(p);
    out.extend(vec![0u8; pad]);
    out
}

#[test]
fn a_server_that_is_not_ssh_is_refused() {
    let port = server(b"HTTP/1.1 400 Bad Request\r\n\r\n".to_vec(), true);
    let e = connect(port);
    assert!(matches!(e, SshError::Protocol(_) | SshError::Io(_)), "{e}");
}

#[test]
fn ssh_1_is_refused_by_name() {
    let port = server(b"SSH-1.5-OldServer\r\n".to_vec(), false);
    let e = connect(port);
    assert!(matches!(&e, SshError::Protocol(m) if m.contains("SSH-1.5")), "{e}");
}

#[test]
fn banner_lines_before_the_version_are_allowed() {
    // RFC 4253 §4.2: other lines may come first. This server then offers only
    // algorithms nobody should use: the negotiation must say so.
    let mut g = b"Welcome to the test box\r\nAuthorized use only\r\nSSH-2.0-Test_1.0\r\n".to_vec();
    g.extend(kexinit("diffie-hellman-group1-sha1", "ssh-dss", "3des-cbc", false));
    let e = connect(server(g, false));
    assert!(matches!(&e, SshError::NoCommonAlgorithm { kind: "key exchange", server } if server == &["diffie-hellman-group1-sha1"]), "{e}");
}

#[test]
fn each_kind_of_algorithm_must_be_agreed() {
    let mut g = b"SSH-2.0-Test_1.0\r\n".to_vec();
    g.extend(kexinit("curve25519-sha256", "ssh-dss", "chacha20-poly1305@openssh.com", false));
    assert!(matches!(connect(server(g, false)), SshError::NoCommonAlgorithm { kind: "host key", .. }));
    let mut g = b"SSH-2.0-Test_1.0\r\n".to_vec();
    g.extend(kexinit("curve25519-sha256", "ssh-ed25519", "aes128-cbc", false));
    assert!(matches!(connect(server(g, false)), SshError::NoCommonAlgorithm { kind: "cipher", .. }));
}

#[test]
fn a_server_hanging_up_mid_handshake_is_an_error_not_a_hang() {
    let mut g = b"SSH-2.0-Test_1.0\r\n".to_vec();
    g.extend(kexinit("curve25519-sha256", "ssh-ed25519", "chacha20-poly1305@openssh.com", false));
    let e = connect(server(g, true));
    assert!(matches!(e, SshError::Io(_)), "{e}");
}

#[test]
fn strict_kex_refuses_a_message_slipped_in_before_kexinit() {
    // The Terrapin attack (CVE-2023-48795) injects a packet before the key exchange;
    // with strict kex the server's KEXINIT must be its very first packet.
    let mut g = b"SSH-2.0-Test_1.0\r\n".to_vec();
    g.extend([0, 0, 0, 12, 10, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // SSH_MSG_IGNORE
    g.extend(kexinit("curve25519-sha256,kex-strict-s-v00@openssh.com", "ssh-ed25519", "chacha20-poly1305@openssh.com", false));
    let e = connect(server(g, false));
    assert!(matches!(&e, SshError::Protocol(m) if m.contains("strict kex")), "{e}");
}

#[test]
fn a_silent_server_hits_the_connect_timeout() {
    let port = server(Vec::new(), false);
    let started = std::time::Instant::now();
    let e = connect(port);
    assert!(matches!(e, SshError::Timeout), "{e}");
    assert!(started.elapsed() < Duration::from_secs(6));
}

/// SSH `string`.
fn s(b: &[u8]) -> Vec<u8> {
    let mut v = (b.len() as u32).to_be_bytes().to_vec();
    v.extend(b);
    v
}

/// An unencrypted packet around `payload`.
fn packet(payload: &[u8]) -> Vec<u8> {
    let mut pad = 8 - (5 + payload.len()) % 8;
    if pad < 4 {
        pad += 8;
    }
    let mut out = ((1 + payload.len() + pad) as u32).to_be_bytes().to_vec();
    out.push(pad as u8);
    out.extend(payload);
    out.extend(vec![0u8; pad]);
    out
}

fn read_packet(sock: &mut std::net::TcpStream) -> Vec<u8> {
    let mut len = [0u8; 4];
    sock.read_exact(&mut len).unwrap();
    let mut rest = vec![0u8; u32::from_be_bytes(len) as usize];
    sock.read_exact(&mut rest).unwrap();
    let pad = rest[0] as usize;
    rest[1..rest.len() - pad].to_vec()
}

/// A server that runs a real curve25519 key exchange with an Ed25519 host key, but
/// signs `what` instead of the exchange hash when `forge` (a man in the middle who
/// shows the real server's key without holding it).
fn kex_server(forge: bool) -> u16 {
    use ring::signature::KeyPair;
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut sock, _) = l.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let v_s = b"SSH-2.0-Fake_1.0";
        sock.write_all(&[&v_s[..], b"\r\n"].concat()).unwrap();
        let mut v_c = Vec::new();
        let mut byte = [0u8];
        loop {
            sock.read_exact(&mut byte).unwrap();
            if byte[0] == b'\n' {
                break;
            }
            v_c.push(byte[0]);
        }
        if v_c.last() == Some(&b'\r') {
            v_c.pop();
        }
        let i_s = {
            let k = kexinit("curve25519-sha256", "ssh-ed25519", "chacha20-poly1305@openssh.com", false);
            sock.write_all(&k).unwrap();
            let pad = k[4] as usize;
            k[5..k.len() - pad].to_vec()
        };
        let i_c = read_packet(&mut sock);
        let init = read_packet(&mut sock);
        assert_eq!(init[0], 30, "ECDH_INIT");
        let q_c = &init[5..];
        let rng = ring::rand::SystemRandom::new();
        let eph = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::X25519, &rng).unwrap();
        let q_s = eph.compute_public_key().unwrap().as_ref().to_vec();
        let shared = ring::agreement::agree_ephemeral(eph, &ring::agreement::UnparsedPublicKey::new(&ring::agreement::X25519, q_c), |k| k.to_vec()).unwrap();
        let host = ring::signature::Ed25519KeyPair::from_pkcs8(ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref()).unwrap();
        let k_s = [s(b"ssh-ed25519"), s(host.public_key().as_ref())].concat();
        // K as an mpint.
        let mut k = shared.clone();
        while k.first() == Some(&0) {
            k.remove(0);
        }
        if k.first().is_some_and(|b| b & 0x80 != 0) {
            k.insert(0, 0);
        }
        let h_input = [s(&v_c), s(v_s), s(&i_c), s(&i_s), s(&k_s), s(q_c), s(&q_s), s(&k)].concat();
        let mut h = ring::digest::digest(&ring::digest::SHA256, &h_input).as_ref().to_vec();
        if forge {
            h[0] ^= 1;
        }
        let sig = [s(b"ssh-ed25519"), s(host.sign(&h).as_ref())].concat();
        let reply = [vec![31u8], s(&k_s), s(&q_s), s(&sig)].concat();
        sock.write_all(&packet(&reply)).unwrap();
        // The honest server goes on to NEWKEYS; then hangs up.
        sock.write_all(&packet(&[21])).unwrap();
        let mut b = [0u8; 1024];
        let _ = sock.read(&mut b);
    });
    port
}

#[test]
fn a_host_key_signature_that_does_not_cover_the_exchange_is_refused() {
    // The forged one: the key is shown, but its holder did not sign this exchange.
    let e = connect(kex_server(true));
    assert!(matches!(&e, SshError::HostKey(m) if m.contains("signature")), "{e}");
    // The same server signing honestly gets past the key exchange (and then hangs up).
    let e = connect(kex_server(false));
    assert!(!matches!(e, SshError::HostKey(_)), "an honest signature must pass: {e}");
}
