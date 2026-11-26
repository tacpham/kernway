//! `AsyncTlsStream` and `client_config` against a real rustls server over
//! loopback, with certificates minted per run: what each `CertCheck` level
//! accepts and refuses (wrong name, unknown CA, expired), mutual TLS, bulk
//! transfer both ways past rustls's internal buffers, clean and unclean
//! shutdown, and the input errors.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::JoinHandle;

use kernway_tls::rustls::pki_types::pem::PemObject;
use kernway_tls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use kernway_tls::rustls::server::WebPkiClientVerifier;
use kernway_tls::rustls::{self, RootCertStore, ServerConfig, ServerConnection, StreamOwned};
use kernway_tls::{client_config, client_config_with_identity, fill_random, AsyncTlsStream, CertCheck, ClientIdentity};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use rt_net::AsyncTcpStream;

// ---------- certificates ----------

struct Ca {
    cert: rcgen::Certificate,
    key: KeyPair,
}

impl Ca {
    fn new(name: &str) -> Self {
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        p.distinguished_name.push(rcgen::DnType::CommonName, name);
        let key = KeyPair::generate().unwrap();
        let cert = p.self_signed(&key).unwrap();
        Self { cert, key }
    }

    fn pem(&self) -> Vec<u8> {
        self.cert.pem().into_bytes()
    }

    /// A leaf for `names`, optionally already expired, for `purpose`.
    fn leaf(&self, names: &[&str], expired: bool, purpose: ExtendedKeyUsagePurpose) -> Leaf {
        let mut p = CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        p.extended_key_usages = vec![purpose];
        if expired {
            p.not_before = rcgen::date_time_ymd(2001, 1, 1);
            p.not_after = rcgen::date_time_ymd(2002, 1, 1);
        }
        let key = KeyPair::generate().unwrap();
        let cert = p.signed_by(&key, &self.cert, &self.key).unwrap();
        Leaf { cert_pem: cert.pem().into_bytes(), key_pem: key.serialize_pem().into_bytes() }
    }

    fn server(&self, names: &[&str]) -> Leaf {
        self.leaf(names, false, ExtendedKeyUsagePurpose::ServerAuth)
    }
}

struct Leaf {
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
}

impl Leaf {
    fn identity(&self) -> ClientIdentity<'_> {
        ClientIdentity { cert_pem: &self.cert_pem, key_pem: &self.key_pem }
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A server config presenting `leaf`, optionally requiring a client
/// certificate issued by `client_ca`.
fn server_config(leaf: &Leaf, client_ca: Option<&Ca>) -> Arc<ServerConfig> {
    let certs = CertificateDer::pem_slice_iter(&leaf.cert_pem).collect::<Result<Vec<_>, _>>().unwrap();
    let key = PrivateKeyDer::from_pem_slice(&leaf.key_pem).unwrap();
    let b = ServerConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().unwrap();
    let b = match client_ca {
        None => b.with_no_client_auth(),
        Some(ca) => {
            let mut roots = RootCertStore::empty();
            roots.add(ca.cert.der().clone()).unwrap();
            let v = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider()).build().unwrap();
            b.with_client_cert_verifier(v)
        }
    };
    Arc::new(b.with_single_cert(certs, key).unwrap())
}

// ---------- the server side: plain blocking rustls on a thread ----------

type Tls = StreamOwned<ServerConnection, TcpStream>;

/// Accept one connection, run the TLS server handshake, then hand the stream to
/// `serve`. The thread's result reports what the server saw.
fn serve<T: Send + 'static>(
    config: Arc<ServerConfig>,
    serve: impl FnOnce(&mut Tls) -> io::Result<T> + Send + 'static,
) -> (SocketAddr, JoinHandle<io::Result<T>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        let mut tls = StreamOwned::new(ServerConnection::new(config).map_err(io::Error::other)?, tcp);
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock)?;
        }
        serve(&mut tls)
    });
    (addr, h)
}

/// Echo one line back, upper-cased.
fn echo_line(tls: &mut Tls) -> io::Result<()> {
    let mut buf = [0u8; 256];
    let n = tls.read(&mut buf)?;
    tls.write_all(&buf[..n].to_ascii_uppercase())?;
    tls.flush()
}

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

async fn dial(addr: SocketAddr, cfg: Arc<rustls::ClientConfig>, name: &str) -> io::Result<AsyncTlsStream> {
    let tcp = AsyncTcpStream::connect(addr).await?;
    AsyncTlsStream::connect(tcp, cfg, name).await
}

async fn read_some(s: &mut AsyncTlsStream) -> io::Result<Vec<u8>> {
    let mut buf = [0u8; 256];
    let n = s.read(&mut buf).await?;
    Ok(buf[..n].to_vec())
}

fn err_text(e: &io::Error) -> String {
    format!("{e} / {e:?}")
}

// ---------- certificate checking ----------

#[test]
fn full_check_accepts_a_cert_from_the_trusted_ca_for_the_right_name() {
    let ca = Ca::new("kw test CA");
    let (addr, server) = serve(server_config(&ca.server(&["localhost"]), None), echo_line);
    run(async {
        let cfg = client_config(Some(&ca.pem()), CertCheck::Full).unwrap();
        let mut s = dial(addr, cfg, "localhost").await.unwrap();
        s.write_all(b"hello tls").await.unwrap();
        assert_eq!(read_some(&mut s).await.unwrap(), b"HELLO TLS");
    });
    server.join().unwrap().unwrap();
}

#[test]
fn full_check_rejects_a_name_the_cert_does_not_carry() {
    let ca = Ca::new("kw test CA");
    let (addr, _server) = serve(server_config(&ca.server(&["db.internal"]), None), echo_line);
    let err = run(async {
        let cfg = client_config(Some(&ca.pem()), CertCheck::Full).unwrap();
        dial(addr, cfg, "localhost").await.err().expect("wrong name must fail")
    });
    assert!(err_text(&err).contains("NotValidForName"), "{}", err_text(&err));
}

#[test]
fn ca_only_accepts_a_wrong_name_but_still_needs_the_ca() {
    let ca = Ca::new("kw test CA");
    let leaf = ca.server(&["db.internal"]);
    let (addr, server) = serve(server_config(&leaf, None), echo_line);
    run(async {
        let cfg = client_config(Some(&ca.pem()), CertCheck::CaOnly).unwrap();
        let mut s = dial(addr, cfg, "localhost").await.unwrap();
        s.write_all(b"ok").await.unwrap();
        assert_eq!(read_some(&mut s).await.unwrap(), b"OK");
    });
    server.join().unwrap().unwrap();

    // Same leaf, but the client trusts a different CA → refused.
    let other = Ca::new("someone else");
    let (addr, _server) = serve(server_config(&leaf, None), echo_line);
    let err = run(async {
        let cfg = client_config(Some(&other.pem()), CertCheck::CaOnly).unwrap();
        dial(addr, cfg, "localhost").await.err().expect("unknown CA must fail")
    });
    assert!(err_text(&err).contains("UnknownIssuer"), "{}", err_text(&err));
}

#[test]
fn the_default_roots_do_not_trust_a_private_ca() {
    let ca = Ca::new("kw test CA");
    let (addr, _server) = serve(server_config(&ca.server(&["localhost"]), None), echo_line);
    let err = run(async { dial(addr, kernway_tls::default_config(), "localhost").await.err().expect("must fail") });
    assert!(err_text(&err).contains("UnknownIssuer"), "{}", err_text(&err));
}

#[test]
fn an_expired_certificate_fails_full_and_ca_only() {
    let ca = Ca::new("kw test CA");
    let leaf = ca.leaf(&["localhost"], true, ExtendedKeyUsagePurpose::ServerAuth);
    for check in [CertCheck::Full, CertCheck::CaOnly] {
        let (addr, _server) = serve(server_config(&leaf, None), echo_line);
        let err = run(async {
            let cfg = client_config(Some(&ca.pem()), check).unwrap();
            dial(addr, cfg, "localhost").await.err().expect("expired must fail")
        });
        assert!(err_text(&err).contains("Expired"), "{check:?}: {}", err_text(&err));
    }
}

#[test]
fn no_check_accepts_anything_but_still_encrypts() {
    // Untrusted CA, wrong name, expired — all accepted with CertCheck::None.
    let stranger = Ca::new("nobody trusts me");
    let leaf = stranger.leaf(&["elsewhere"], true, ExtendedKeyUsagePurpose::ServerAuth);
    let (addr, server) = serve(server_config(&leaf, None), |tls: &mut Tls| {
        echo_line(tls)?;
        // The server sees a real negotiated TLS session.
        Ok(tls.conn.negotiated_cipher_suite().is_some())
    });
    run(async {
        let cfg = client_config(None, CertCheck::None).unwrap();
        let mut s = dial(addr, cfg, "localhost").await.unwrap();
        s.write_all(b"anyone").await.unwrap();
        assert_eq!(read_some(&mut s).await.unwrap(), b"ANYONE");
    });
    assert!(server.join().unwrap().unwrap());
}

// ---------- mutual TLS ----------

#[test]
fn mutual_tls_presents_the_client_certificate() {
    let ca = Ca::new("kw test CA");
    let client = ca.leaf(&["app-client"], false, ExtendedKeyUsagePurpose::ClientAuth);
    let (addr, server) = serve(server_config(&ca.server(&["localhost"]), Some(&ca)), |tls: &mut Tls| {
        echo_line(tls)?;
        Ok(tls.conn.peer_certificates().map(<[_]>::len))
    });
    run(async {
        let cfg = client_config_with_identity(Some(&ca.pem()), CertCheck::Full, Some(client.identity())).unwrap();
        let mut s = dial(addr, cfg, "localhost").await.unwrap();
        s.write_all(b"me").await.unwrap();
        assert_eq!(read_some(&mut s).await.unwrap(), b"ME");
    });
    assert_eq!(server.join().unwrap().unwrap(), Some(1), "the server received our certificate");
}

#[test]
fn a_server_requiring_a_client_certificate_refuses_a_client_without_one() {
    let ca = Ca::new("kw test CA");
    let (addr, server) = serve(server_config(&ca.server(&["localhost"]), Some(&ca)), echo_line);
    let outcome = run(async {
        let cfg = client_config(Some(&ca.pem()), CertCheck::Full).unwrap();
        // TLS 1.3: the client finishes its side first; the refusal arrives as
        // an alert on the next read.
        match dial(addr, cfg, "localhost").await {
            Err(e) => Err(e),
            Ok(mut s) => {
                let _ = s.write_all(b"hi").await;
                read_some(&mut s).await
            }
        }
    });
    let err = outcome.expect_err("the connection must not carry data");
    assert!(err_text(&err).contains("CertificateRequired"), "{}", err_text(&err));
    assert!(server.join().unwrap().is_err(), "the server refused the handshake");
}

#[test]
fn a_client_certificate_from_another_ca_is_refused() {
    let ca = Ca::new("kw test CA");
    let rogue = Ca::new("rogue").leaf(&["app-client"], false, ExtendedKeyUsagePurpose::ClientAuth);
    let (addr, server) = serve(server_config(&ca.server(&["localhost"]), Some(&ca)), echo_line);
    let outcome = run(async {
        let cfg = client_config_with_identity(Some(&ca.pem()), CertCheck::Full, Some(rogue.identity())).unwrap();
        let mut s = dial(addr, cfg, "localhost").await?;
        let _ = s.write_all(b"hi").await;
        read_some(&mut s).await
    });
    assert!(outcome.is_err(), "{outcome:?}");
    assert!(server.join().unwrap().is_err());
}

#[test]
fn bad_identity_pem_is_invalid_data() {
    let ca = Ca::new("kw test CA");
    let client = ca.leaf(&["c"], false, ExtendedKeyUsagePurpose::ClientAuth);
    let no_cert = ClientIdentity { cert_pem: b"", key_pem: &client.key_pem };
    let no_key = ClientIdentity { cert_pem: &client.cert_pem, key_pem: b"-----BEGIN PRIVATE KEY-----\nnope\n-----END PRIVATE KEY-----\n" };
    for id in [no_cert, no_key] {
        let e = client_config_with_identity(Some(&ca.pem()), CertCheck::Full, Some(id)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }
    // A key that does not match the certificate is refused too.
    let other_key = ca.leaf(&["d"], false, ExtendedKeyUsagePurpose::ClientAuth).key_pem;
    let mismatched = ClientIdentity { cert_pem: &client.cert_pem, key_pem: &other_key };
    let e = client_config_with_identity(Some(&ca.pem()), CertCheck::Full, Some(mismatched)).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    assert!(e.to_string().contains("KeyMismatch"), "{e}");
}

// ---------- data transfer ----------

fn pattern(len: usize, seed: u32) -> Vec<u8> {
    (0..len as u32).map(|i| (i.wrapping_mul(2654435761).wrapping_add(seed) >> 13) as u8).collect()
}

#[test]
fn megabytes_both_ways_arrive_intact() {
    const UP: usize = 3 << 20; // past rustls's 64 KiB unsent-plaintext limit
    const DOWN: usize = 8 << 20; // a fast burst, past its received-plaintext limit
    let ca = Ca::new("kw test CA");
    let (addr, server) = serve(server_config(&ca.server(&["localhost"]), None), |tls: &mut Tls| {
        let mut got = vec![0u8; UP];
        tls.read_exact(&mut got)?;
        tls.write_all(&pattern(DOWN, 7))?;
        tls.conn.send_close_notify();
        tls.flush()?;
        Ok(got == pattern(UP, 3))
    });
    run(async {
        let cfg = client_config(Some(&ca.pem()), CertCheck::Full).unwrap();
        let mut s = dial(addr, cfg, "localhost").await.unwrap();
        s.write_all(&pattern(UP, 3)).await.unwrap();
        let mut down = Vec::with_capacity(DOWN);
        let mut buf = vec![0u8; 50_000];
        loop {
            let n = s.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            down.extend_from_slice(&buf[..n]);
        }
        assert_eq!(down.len(), DOWN);
        assert!(down == pattern(DOWN, 7), "download corrupted");
    });
    assert!(server.join().unwrap().unwrap(), "upload corrupted");
}

#[test]
fn small_reads_drain_a_large_record_piece_by_piece() {
    let ca = Ca::new("kw test CA");
    let (addr, server) = serve(server_config(&ca.server(&["localhost"]), None), |tls: &mut Tls| {
        tls.write_all(&pattern(40_000, 1))?;
        tls.flush()
    });
    run(async {
        let cfg = client_config(Some(&ca.pem()), CertCheck::Full).unwrap();
        let mut s = dial(addr, cfg, "localhost").await.unwrap();
        let mut got = Vec::new();
        let mut one = [0u8; 7];
        while got.len() < 40_000 {
            let n = s.read(&mut one).await.unwrap();
            assert!(n > 0);
            got.extend_from_slice(&one[..n]);
        }
        assert!(got == pattern(40_000, 1));
    });
    server.join().unwrap().unwrap();
}

#[test]
fn close_notify_and_a_bare_tcp_close_both_end_the_stream() {
    let ca = Ca::new("kw test CA");
    for polite in [true, false] {
        let (addr, server) = serve(server_config(&ca.server(&["localhost"]), None), move |tls: &mut Tls| {
            tls.write_all(b"bye")?;
            if polite {
                tls.conn.send_close_notify();
            }
            tls.flush()?;
            tls.sock.shutdown(std::net::Shutdown::Both)
        });
        run(async {
            let cfg = client_config(Some(&ca.pem()), CertCheck::Full).unwrap();
            let mut s = dial(addr, cfg, "localhost").await.unwrap();
            assert_eq!(read_some(&mut s).await.unwrap(), b"bye");
            assert_eq!(read_some(&mut s).await.unwrap(), b"", "polite={polite}");
            assert_eq!(read_some(&mut s).await.unwrap(), b"", "stays at end of stream");
        });
        server.join().unwrap().unwrap();
    }
}

// ---------- failures before any TLS ----------

#[test]
fn a_peer_that_hangs_up_during_the_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let h = std::thread::spawn(move || drop(listener.accept().unwrap()));
    let err = run(async {
        let cfg = client_config(None, CertCheck::None).unwrap();
        dial(addr, cfg, "localhost").await.err().expect("must fail")
    });
    h.join().unwrap();
    // A plain close is EOF; Windows may report the reset instead.
    assert!(
        matches!(err.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted),
        "{}",
        err_text(&err)
    );
}

#[test]
fn a_peer_that_is_not_speaking_tls() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        let (mut tcp, _) = listener.accept().unwrap();
        let _ = tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        let mut sink = [0u8; 1024];
        let _ = tcp.read(&mut sink);
    });
    let err = run(async {
        let cfg = client_config(None, CertCheck::None).unwrap();
        dial(addr, cfg, "localhost").await.err().expect("must fail")
    });
    h.join().unwrap();
    assert!(err_text(&err).contains("InvalidContentType") || err_text(&err).contains("InvalidMessage"), "{}", err_text(&err));
}

#[test]
fn an_invalid_server_name_is_invalid_input() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let err = run(async {
        let cfg = client_config(None, CertCheck::None).unwrap();
        dial(addr, cfg, "not a host name!").await.err().expect("must fail")
    });
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn an_ip_address_is_a_valid_server_name() {
    let ca = Ca::new("kw test CA");
    let (addr, server) = serve(server_config(&ca.server(&["127.0.0.1"]), None), echo_line);
    run(async {
        let cfg = client_config(Some(&ca.pem()), CertCheck::Full).unwrap();
        let mut s = dial(addr, cfg, "127.0.0.1").await.unwrap();
        s.write_all(b"ip").await.unwrap();
        assert_eq!(read_some(&mut s).await.unwrap(), b"IP");
    });
    server.join().unwrap().unwrap();
}

#[test]
fn a_ca_bundle_with_several_certificates_trusts_each() {
    let (a, b) = (Ca::new("A"), Ca::new("B"));
    let mut bundle = a.pem();
    bundle.extend_from_slice(&b.pem());
    let cfg = client_config(Some(&bundle), CertCheck::Full).unwrap();
    let (addr, server) = serve(server_config(&b.server(&["localhost"]), None), echo_line);
    run(async {
        let mut s = dial(addr, cfg, "localhost").await.unwrap();
        s.write_all(b"b").await.unwrap();
        assert_eq!(read_some(&mut s).await.unwrap(), b"B");
    });
    server.join().unwrap().unwrap();
}

#[test]
fn fill_random_gives_fresh_bytes() {
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    fill_random(&mut a).unwrap();
    fill_random(&mut b).unwrap();
    assert_ne!(a, [0u8; 32]);
    assert_ne!(a, b);
    fill_random(&mut []).unwrap();
}
