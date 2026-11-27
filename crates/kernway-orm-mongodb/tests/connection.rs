//! Connecting: authentication mechanisms, TLS and X.509, compression, the
//! Unix socket, timeouts, server selection errors, `mongodb+srv://` (against
//! an in-process DNS server), and host names resolved off the reactor.

mod common;
use common::{block, env};
use kernway_orm_mongodb::conn::Connection;
use kernway_orm_mongodb::*;
use std::time::{Duration, Instant};

fn with_creds(url: &str, creds: &str) -> String {
    url.replacen("kernway:kernway_pw", creds, 1)
}

async fn whoami(c: &Client) -> String {
    let r = c.run_command("admin", doc! { "connectionStatus" => 1 }).await.unwrap();
    let users = r.get_document("authInfo").and_then(|a| a.get_array("authenticatedUsers")).cloned().unwrap_or_default();
    users.first().and_then(|u| u.as_document()?.get_str("user").map(String::from)).unwrap_or_default()
}

#[test]
fn scram_mechanisms_and_wrong_passwords() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        for (creds, mech, user) in [
            ("kernway:kernway_pw", "", "kernway"),                          // negotiated: SCRAM-SHA-256, speculative
            ("kernway:kernway_pw", "&authMechanism=SCRAM-SHA-1", "kernway"),
            ("kw_sha1:sha1_pw", "", "kw_sha1"),                           // negotiated down to SHA-1
            ("kw_sha256:sha256_pw", "&authMechanism=SCRAM-SHA-256", "kw_sha256"),
            ("kw%3Asha256:x", "", ""),                                      // (no such user: below)
        ] {
            if user.is_empty() {
                continue;
            }
            let c = Client::connect(&format!("{}{mech}", with_creds(&url, creds))).await.unwrap();
            assert_eq!(whoami(&c).await, user, "{creds}{mech}");
        }
        for (creds, mech) in [("kernway:nope", ""), ("kw_sha1:nope", ""), ("kw_sha256:sha256_pw", "&authMechanism=SCRAM-SHA-1"), ("nobody:x", "")] {
            let o = ClientOptions::parse(&format!("{}{mech}", with_creds(&url, creds))).unwrap();
            let err = Connection::connect_for_test(&o).await.err().unwrap_or_else(|| panic!("{creds}{mech} accepted"));
            assert_eq!(err.kind, ErrorKind::Auth, "{creds}{mech}: {err}");
        }
        // The client surfaces an auth failure on the first operation, clearly.
        let c = Client::connect(&with_creds(&url, "kernway:nope")).await.unwrap();
        let err = c.database("x").collection::<Document>("y").count_documents(doc! {}).await.unwrap_err();
        assert!(err.to_string().contains("Authentication failed"), "{err}");
    });
}

/// Percent-encode a URL user-info component.
fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

#[test]
fn scram_sha256_passwords_are_saslprepped() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        // The server stores the SASLprep form: the soft hyphen vanishes,
        // U+2168 is "IX", the Vietnamese letter may come composed or not.
        let admin = Client::connect(&url).await.unwrap();
        let _ = admin.run_command("admin", doc! { "dropUser" => "kw_prep" }).await;
        admin
            .run_command(
                "admin",
                doc! { "createUser" => "kw_prep", "pwd" => "pw\u{AD}\u{2168}-kh\u{1EA9}u", "roles" => Vec::<Bson>::new(), "mechanisms" => vec![Bson::from("SCRAM-SHA-256")] },
            )
            .await
            .unwrap();
        for pw in ["pw\u{AD}\u{2168}-kh\u{1EA9}u", "pwIX-kh\u{1EA9}u", "pw\u{2168}-kha\u{302}\u{309}u"] {
            let o = ClientOptions::parse(&with_creds(&url, &format!("kw_prep:{}", pct(pw)))).unwrap();
            Connection::connect_for_test(&o).await.unwrap_or_else(|err| panic!("{pw:?}: {err}"));
        }
        for pw in ["pwIX-khau", "pw-\u{1F600}"] {
            let o = ClientOptions::parse(&with_creds(&url, &format!("kw_prep:{}", pct(pw)))).unwrap();
            let err = Connection::connect_for_test(&o).await.err().unwrap_or_else(|| panic!("{pw:?} accepted"));
            assert_eq!(err.kind, ErrorKind::Auth, "{pw:?}: {err}");
        }
        // No SASLprep form (an emoji is unassigned in Unicode 3.2): refused
        // before anything is sent.
        let o = ClientOptions::parse(&with_creds(&url, &format!("kw_prep:{}", pct("pw-\u{1F600}")))).unwrap();
        let err = Connection::connect_for_test(&o).await.err().unwrap();
        assert!(err.to_string().contains("SASLprep"), "{err}");
        admin.run_command("admin", doc! { "dropUser" => "kw_prep" }).await.unwrap();
    });
}

#[test]
fn compression_is_negotiated_and_used() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        for (opt, want) in [("&compressors=snappy", Some(Compressor::Snappy)), ("&compressors=zlib&zlibCompressionLevel=9", Some(Compressor::Zlib(9))), ("", None)] {
            let o = ClientOptions::parse(&format!("{url}{opt}")).unwrap();
            let mut conn = Connection::connect_for_test(&o).await.unwrap();
            assert_eq!(conn.compressor(), want, "{opt}");
            // A large, compressible document both ways.
            let big = "x".repeat(4 << 20);
            let r = conn.command("admin", doc! { "ping" => 1, "comment" => big.clone() }).await.unwrap();
            assert_eq!(r.get_i64("ok"), Some(1));
            let c = Client::connect(&format!("{url}{opt}")).await.unwrap();
            let coll = c.database("kernway_test").collection::<Document>(&common::unique("compress"));
            coll.insert_one(&doc! { "_id" => 1, "s" => big.clone() }).await.unwrap();
            let got = coll.find_one(doc! { "_id" => 1 }, FindOptions::default()).await.unwrap().unwrap();
            assert_eq!(got.get_str("s").map(str::len), Some(big.len()));
            coll.drop().await.unwrap();
        }
    });
}

#[test]
fn tls_certificates_and_x509() {
    let Some(dir) = env("KERNWAY_MONGO_TLS_DIR") else { return };
    block(async move {
        let ca = format!("{dir}/ca.pem");
        let client_pem = format!("{dir}/client.pem");
        let x509 = format!("mongodb://localhost:27101/?tls=true&tlsCAFile={ca}&tlsCertificateKeyFile={client_pem}&authMechanism=MONGODB-X509&directConnection=true");
        let c = Client::connect(&x509).await.unwrap();
        let who = whoami(&c).await;
        assert!(who.contains("CN=kw_x509"), "authenticated by the certificate: {who:?}");
        // SCRAM over TLS.
        let scram = format!("mongodb://root:root_pw@localhost:27101/?tls=true&tlsCAFile={ca}&directConnection=true");
        let o = ClientOptions::parse(&scram).unwrap();
        let conn = Connection::connect_for_test(&o).await.unwrap();
        assert!(conn.is_tls());
        // The server certificate names localhost only.
        let wrong_name = format!("mongodb://root:root_pw@127.0.0.1:27101/?tls=true&tlsCAFile={ca}&directConnection=true");
        let err = Connection::connect_for_test(&ClientOptions::parse(&wrong_name).unwrap()).await.err().unwrap();
        assert!(err.to_string().contains("TLS"), "{err}");
        let lenient = format!("{wrong_name}&tlsAllowInvalidHostnames=true");
        assert!(Connection::connect_for_test(&ClientOptions::parse(&lenient).unwrap()).await.is_ok());
        // Without our CA the chain does not verify; tlsInsecure accepts anything.
        let no_ca = "mongodb://root:root_pw@localhost:27101/?tls=true&directConnection=true";
        assert!(Connection::connect_for_test(&ClientOptions::parse(no_ca).unwrap()).await.is_err());
        assert!(Connection::connect_for_test(&ClientOptions::parse(&format!("{no_ca}&tlsInsecure=true")).unwrap()).await.is_ok());
        // Plaintext against a TLS-only server fails, without hanging.
        let started = Instant::now();
        let plain = "mongodb://root:root_pw@localhost:27101/?directConnection=true&connectTimeoutMS=3000";
        assert!(Connection::connect_for_test(&ClientOptions::parse(plain).unwrap()).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
        // A missing CA file is a clear configuration error.
        let err = Client::connect("mongodb://localhost:27101/?tls=true&tlsCAFile=/no/such/ca.pem").await.err().unwrap();
        assert!(err.to_string().contains("tlsCAFile"), "{err}");
    });
}

#[test]
fn unix_domain_socket() {
    if cfg!(windows) {
        eprintln!("n/a: no Unix-domain sockets on Windows");
        return;
    }
    let Some(sock) = env("KERNWAY_MONGO_SOCKET") else { return };
    block(async move {
        let url = format!("mongodb://kernway:kernway_pw@{}/?authSource=admin", sock.replace('/', "%2F"));
        let c = Client::connect(&url).await.unwrap();
        assert_eq!(whoami(&c).await, "kernway");
        assert!(matches!(c.topology().servers.keys().next(), Some(Address::Unix(_))));
    });
}

#[test]
fn timeouts_and_selection_errors_are_bounded() {
    block(async move {
        // Nothing listens: selection gives up after serverSelectionTimeoutMS,
        // naming the server and why.
        let c = Client::connect("mongodb://127.0.0.1:1/?serverSelectionTimeoutMS=800&connectTimeoutMS=500").await.unwrap();
        let started = Instant::now();
        let err = c.ping().await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert_eq!(err.kind, ErrorKind::ServerSelection);
        assert!(err.to_string().contains("127.0.0.1:1"), "{err}");
        // An address that never answers: the connect timeout bounds it.
        let o = ClientOptions::parse("mongodb://10.255.255.1:27017/?connectTimeoutMS=700").unwrap();
        let started = Instant::now();
        assert!(Connection::connect_for_test(&o).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    });
}

#[test]
fn host_names_resolve_off_the_reactor() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        let ticks = std::rc::Rc::new(std::cell::Cell::new(0));
        let t = ticks.clone();
        rt_core::spawn(async move {
            for _ in 0..5 {
                rt_core::sleep(Duration::from_millis(1)).await;
                t.set(t.get() + 1);
            }
        });
        let c = Client::connect(&url.replace("127.0.0.1", "localhost")).await.unwrap();
        c.ping().await.unwrap();
        let started = Instant::now();
        while ticks.get() < 5 && started.elapsed() < Duration::from_secs(2) {
            rt_core::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(ticks.get(), 5);
        let c = Client::connect("mongodb://no-such-host.invalid/?serverSelectionTimeoutMS=2000").await.unwrap();
        let err = c.ping().await.unwrap_err();
        assert!(err.to_string().contains("no-such-host.invalid"), "{err}");
    });
}

/// A DNS server over TCP answering SRV / TXT for `*.kw.test`.
async fn fake_dns(listener: &mut rt_net::AsyncTcpListener) {
    loop {
        let Ok((mut s, _)) = listener.accept().await else { return };
        rt_core::spawn(async move {
            let mut buf = vec![0u8; 512];
            let mut got = 0;
            while got < 2 || got < 2 + u16::from_be_bytes([buf[0], buf[1]]) as usize {
                match s.read(&mut buf[got..]).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => got += n,
                }
            }
            let q = &buf[2..got];
            let id = [q[0], q[1]];
            // Question name.
            let mut pos = 12;
            let mut labels = Vec::new();
            while q[pos] != 0 {
                let n = q[pos] as usize;
                labels.push(String::from_utf8_lossy(&q[pos + 1..pos + 1 + n]).into_owned());
                pos += 1 + n;
            }
            let qtype = u16::from_be_bytes([q[pos + 1], q[pos + 2]]);
            let question = &q[12..pos + 5];
            let name = labels.join(".");
            let mut answers: Vec<(u16, Vec<u8>)> = Vec::new();
            let enc = |n: &str| {
                let mut v = Vec::new();
                for l in n.split('.') {
                    v.push(l.len() as u8);
                    v.extend_from_slice(l.as_bytes());
                }
                v.push(0);
                v
            };
            match (qtype, name.as_str()) {
                (33, "_mongodb._tcp.rs.kw.test") => {
                    for (prio, port, host) in [(1u16, 27111u16, "a.kw.test"), (1, 27112, "b.kw.test")] {
                        let mut r = Vec::new();
                        r.extend_from_slice(&prio.to_be_bytes());
                        r.extend_from_slice(&5u16.to_be_bytes());
                        r.extend_from_slice(&port.to_be_bytes());
                        r.extend_from_slice(&enc(host));
                        answers.push((33, r));
                    }
                }
                (33, "_mongodb._tcp.evil.kw.test") => {
                    let mut r = vec![0, 1, 0, 0, 0x69, 0x8f];
                    r.extend_from_slice(&enc("mongo.attacker.example"));
                    answers.push((33, r));
                }
                (16, "rs.kw.test") => {
                    let t = b"replicaSet=rs0&authSource=other";
                    let mut r = vec![t.len() as u8];
                    r.extend_from_slice(t);
                    answers.push((16, r));
                }
                _ => {}
            }
            let rcode: u16 = if answers.is_empty() && !name.starts_with("_mongodb") && qtype == 33 { 3 } else { 0 };
            let mut m = Vec::new();
            m.extend_from_slice(&id);
            m.extend_from_slice(&(0x8180u16 | rcode).to_be_bytes());
            m.extend_from_slice(&1u16.to_be_bytes());
            m.extend_from_slice(&(answers.len() as u16).to_be_bytes());
            m.extend_from_slice(&[0, 0, 0, 0]);
            m.extend_from_slice(question);
            for (t, rd) in answers {
                m.extend_from_slice(&[0xc0, 12]);
                m.extend_from_slice(&t.to_be_bytes());
                m.extend_from_slice(&1u16.to_be_bytes());
                m.extend_from_slice(&300u32.to_be_bytes());
                m.extend_from_slice(&(rd.len() as u16).to_be_bytes());
                m.extend_from_slice(&rd);
            }
            let mut out = (m.len() as u16).to_be_bytes().to_vec();
            out.extend_from_slice(&m);
            let _ = s.write_all(&out).await;
        });
    }
}

#[test]
fn srv_connection_strings() {
    block(async move {
        let mut listener = rt_net::AsyncTcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = listener.local_addr().unwrap().port();
        // The variable is read at query time; the test owns this process's env.
        unsafe { std::env::set_var("KERNWAY_DNS_SERVER", format!("127.0.0.1:{port}")) };
        rt_core::spawn(async move { fake_dns(&mut listener).await });
        let mut o = ClientOptions::parse("mongodb+srv://u:p@rs.kw.test/db?retryWrites=false").unwrap();
        o.resolve_srv().await.unwrap();
        assert_eq!(o.hosts, [Address::Tcp("a.kw.test".into(), 27111), Address::Tcp("b.kw.test".into(), 27112)]);
        assert_eq!(o.replica_set.as_deref(), Some("rs0"), "from TXT");
        assert_eq!(o.credential.as_ref().unwrap().source, "other", "authSource from TXT");
        assert!(o.tls.is_some(), "SRV implies TLS");
        let mut o = ClientOptions::parse("mongodb+srv://rs.kw.test/?srvMaxHosts=1&tls=false").unwrap();
        o.resolve_srv().await.unwrap();
        assert_eq!(o.hosts.len(), 1);
        assert!(o.tls.is_none());
        // Targets must lie inside the SRV name's domain.
        let mut o = ClientOptions::parse("mongodb+srv://evil.kw.test/").unwrap();
        let err = o.resolve_srv().await.unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
        // No records at all.
        let mut o = ClientOptions::parse("mongodb+srv://none.kw.test/").unwrap();
        assert!(o.resolve_srv().await.is_err());
        assert!(ClientOptions::parse("mongodb+srv://kw.test/").is_ok());
        let mut o = ClientOptions::parse("mongodb+srv://kw.test/").unwrap();
        assert!(o.resolve_srv().await.unwrap_err().to_string().contains("three parts"));
        unsafe { std::env::remove_var("KERNWAY_DNS_SERVER") };
    });
}
