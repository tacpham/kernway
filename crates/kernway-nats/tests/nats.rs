//! The NATS client against live servers — started by
//! `tests/support/nats-setup.sh`, which prints `KERNWAY_NATS_DIR` (certs) and
//! the ports below. Without them each test reports `skip:`, which the CI
//! job and the acceptance run treat as a failure.

use kernway_messaging::{Broker, Consumer, MsgError, OutMessage, Producer, Subscription};
use kernway_nats::{Auth, Client, NatsBroker, Options, TlsOptions};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The servers are up (`tests/support/nats-setup.sh` printed the env);
/// otherwise the test says `skip:` — the CI job and the acceptance run
/// count that as a failure.
fn live() -> bool {
    let up = std::env::var_os("KERNWAY_NATS_DIR").is_some();
    if !up {
        eprintln!("skip: KERNWAY_NATS_DIR not set (tests/support/nats-setup.sh)");
    }
    up
}


fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn dir() -> PathBuf {
    PathBuf::from(std::env::var("KERNWAY_NATS_DIR").expect("KERNWAY_NATS_DIR"))
}

fn opts(port: u16) -> Options {
    Options { servers: vec![("127.0.0.1".into(), port)], ..Options::default() }
}

fn tls_opts(port: u16, t: TlsOptions) -> Options {
    Options { servers: vec![("localhost".into(), port)], tls: Some(t), ..Options::default() }
}

fn ca() -> TlsOptions {
    TlsOptions { ca_file: Some(dir().join("ca.pem")), ..TlsOptions::default() }
}

fn unique(name: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64;
    format!("kwt-{name}-{}-{:x}-{}", std::process::id(), t & 0xff_ffff, N.fetch_add(1, Ordering::Relaxed))
}

/// A TCP proxy to a server: connections can be cut, and the proxy can play
/// a dead server (accepts, then closes at once).
struct Proxy {
    port: u16,
    down: Arc<AtomicBool>,
    conns: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    fn to(target: u16) -> Proxy {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let down = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let (d, c) = (down.clone(), conns.clone());
        std::thread::spawn(move || {
            for client in l.incoming() {
                let Ok(client) = client else { continue };
                if d.load(Ordering::SeqCst) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let Ok(server) = TcpStream::connect(("127.0.0.1", target)) else { continue };
                c.lock().unwrap().extend([client.try_clone().unwrap(), server.try_clone().unwrap()]);
                for (mut from, mut to) in [(client.try_clone().unwrap(), server.try_clone().unwrap()), (server, client)] {
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 16384];
                        while let Ok(n) = from.read(&mut buf) {
                            if n == 0 || to.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                        let _ = to.shutdown(Shutdown::Both);
                        let _ = from.shutdown(Shutdown::Both);
                    });
                }
            }
        });
        Proxy { port, down, conns }
    }

    /// Drop every open connection.
    fn cut(&self) {
        for s in self.conns.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// Play a dead server (or come back).
    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
        if down {
            self.cut();
        }
    }
}

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        rt_core::sleep(Duration::from_millis(20)).await;
    }
}

#[test]
fn publish_subscribe_wildcards_and_headers() {
    if !live() {
        return;
    }
    block(async {
        let c = Client::connect(opts(4222)).await.unwrap();
        let base = unique("ps");
        let mut one = c.subscribe(&format!("{base}.*"), None).unwrap();
        let mut all = c.subscribe(&format!("{base}.>"), None).unwrap();
        c.flush().await.unwrap();
        let h = vec![("Trace".to_string(), "t-1".to_string()), ("Multi".to_string(), "a".to_string()), ("Multi".to_string(), "b".to_string())];
        c.publish(&format!("{base}.a"), None, &h, b"with headers").unwrap();
        c.publish(&format!("{base}.a.b"), None, &[], b"deep").unwrap();
        c.publish(&format!("{base}.e"), None, &[], b"").unwrap();
        let m = one.next_timeout(Duration::from_secs(5)).await.unwrap();
        assert_eq!((m.subject.as_str(), m.payload.as_slice()), (format!("{base}.a").as_str(), &b"with headers"[..]));
        assert_eq!(m.header("trace"), Some("t-1"));
        assert_eq!(m.headers.iter().filter(|(k, _)| k == "Multi").count(), 2, "repeated headers survive");
        let e = one.next_timeout(Duration::from_secs(5)).await.unwrap();
        assert_eq!((e.subject, e.payload), (format!("{base}.e"), vec![]), "`*` does not match a.b; empty payload");
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(all.next_timeout(Duration::from_secs(5)).await.unwrap().subject);
        }
        assert_eq!(got, [format!("{base}.a"), format!("{base}.a.b"), format!("{base}.e")]);
        assert!(matches!(one.next_timeout(Duration::from_millis(300)).await, Err(MsgError::Timeout)));
        // Unsubscribed on drop: nothing more arrives for it, the other still works.
        drop(one);
        c.publish(&format!("{base}.z"), None, &[], b"z").unwrap();
        assert_eq!(all.next_timeout(Duration::from_secs(5)).await.unwrap().payload, b"z");
        for bad in ["", "a b", "a..b"] {
            assert!(matches!(c.publish(bad, None, &[], b""), Err(MsgError::Config(_))), "{bad:?}");
        }
    });
}

#[test]
fn queue_groups_split_the_messages() {
    if !live() {
        return;
    }
    block(async {
        let c = Client::connect(opts(4222)).await.unwrap();
        let subj = unique("q");
        let mut a = c.subscribe(&subj, Some("workers")).unwrap();
        let mut b = c.subscribe(&subj, Some("workers")).unwrap();
        let mut watcher = c.subscribe(&subj, None).unwrap();
        c.flush().await.unwrap();
        for i in 0..200 {
            c.publish(&subj, None, &[], format!("{i}").as_bytes()).unwrap();
        }
        c.flush().await.unwrap();
        let mut counts = [0usize; 3];
        for (i, s) in [&mut a, &mut b, &mut watcher].into_iter().enumerate() {
            while s.next_timeout(Duration::from_millis(400)).await.is_ok() {
                counts[i] += 1;
            }
        }
        assert_eq!(counts[0] + counts[1], 200, "each message to one member: {counts:?}");
        assert!(counts[0] > 0 && counts[1] > 0, "both members worked: {counts:?}");
        assert_eq!(counts[2], 200, "a plain subscriber sees everything");
    });
}

#[test]
fn request_reply_and_no_responders() {
    if !live() {
        return;
    }
    block(async {
        let c = Client::connect(opts(4222)).await.unwrap();
        let svc = unique("svc");
        let mut rx = c.subscribe(&svc, None).unwrap();
        c.flush().await.unwrap();
        let responder = c.clone();
        rt_core::spawn(async move {
            while let Ok(m) = rx.next().await {
                let mut out = b"echo:".to_vec();
                out.extend_from_slice(&m.payload);
                let _ = responder.publish(m.reply.as_deref().unwrap(), None, &[("Served-By".into(), "kw".into())], &out);
            }
        });
        // Many concurrent requests share one inbox subscription.
        let results: std::rc::Rc<std::cell::RefCell<Vec<Option<Vec<u8>>>>> = std::rc::Rc::new(std::cell::RefCell::new(vec![None; 50]));
        for i in 0..50 {
            let (c, svc, results) = (c.clone(), svc.clone(), results.clone());
            rt_core::spawn(async move {
                let m = c.request(&svc, &[], format!("{i}").as_bytes(), Duration::from_secs(5)).await.unwrap();
                assert_eq!(m.header("served-by"), Some("kw"));
                results.borrow_mut()[i] = Some(m.payload);
            });
        }
        wait_until("50 concurrent replies", || results.borrow().iter().all(Option::is_some)).await;
        for (i, r) in results.borrow().iter().enumerate() {
            assert_eq!(r.as_deref(), Some(format!("echo:{i}").as_bytes()), "each reply reached its own request");
        }
        let started = Instant::now();
        let err = c.request(&unique("nobody"), &[], b"?", Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(&err, MsgError::Broker(m) if m.contains("no responders")), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2), "no-responders is immediate, not a timeout");
    });
}

#[test]
fn payload_limit_is_enforced_before_sending() {
    if !live() {
        return;
    }
    block(async {
        let c = Client::connect(opts(4222)).await.unwrap();
        let max = c.server_info()["max_payload"].as_u64().unwrap() as usize;
        assert_eq!(max, 1 << 20);
        let subj = unique("big");
        let mut s = c.subscribe(&subj, None).unwrap();
        c.flush().await.unwrap();
        let ok = vec![7u8; max - 100];
        c.publish(&subj, None, &[], &ok).unwrap();
        assert_eq!(s.next_timeout(Duration::from_secs(5)).await.unwrap().payload.len(), max - 100);
        assert!(matches!(c.publish(&subj, None, &[], &vec![0u8; max + 1]), Err(MsgError::Broker(m)) if m.contains("max_payload")));
        c.flush().await.unwrap();
        assert!(c.is_connected(), "the connection survives a refused publish");
    });
}

#[test]
fn authentication_user_password_token_and_nkey() {
    if !live() {
        return;
    }
    block(async {
        let ok = |o: Options| async move { Client::connect(o).await.map(|c| c.is_connected()) };
        let with = |port, auth| Options { auth, ..opts(port) };
        assert_eq!(ok(with(4223, Auth::UserPassword("kw".into(), "kw_pw".into()))).await, Ok(true));
        let bad = ok(with(4223, Auth::UserPassword("kw".into(), "wrong".into()))).await.unwrap_err();
        assert!(matches!(bad, MsgError::Auth(_)), "{bad}");
        assert!(matches!(ok(opts(4223)).await, Err(MsgError::Auth(_))), "no credentials");
        let nkey = Auth::NKey("SUACSSL3UAHUDXKFSNVUZRF5UHPMWZ6BFDTJ7M6USDXIEDNPPQYYYCU3VY".into());
        assert_eq!(ok(with(4223, nkey)).await, Ok(true), "NKey: the nonce signed with the seed");
        // A valid seed the server does not know.
        let stranger = Auth::NKey("SUAMLK2ZNL35WSMW37E7UD4VZ7ELPKW7DHC3BWBSD2GCZ7IUQQXZIORRBU".into());
        assert!(matches!(ok(with(4223, stranger)).await, Err(MsgError::Auth(_))));
        assert_eq!(ok(Options::parse("nats://s3cr3t@127.0.0.1:4224").unwrap()).await, Ok(true));
        assert!(matches!(ok(Options::parse("nats://nope@127.0.0.1:4224").unwrap()).await, Err(MsgError::Auth(_))));
        // Through a URL with credentials.
        let c = Client::connect_url("nats://kw:kw_pw@127.0.0.1:4223").await.unwrap();
        let s = unique("auth");
        let mut sub = c.subscribe(&s, None).unwrap();
        c.flush().await.unwrap();
        c.publish(&s, None, &[], b"in").unwrap();
        assert_eq!(sub.next_timeout(Duration::from_secs(5)).await.unwrap().payload, b"in");
    });
}

#[test]
fn tls_required_handshake_first_and_mutual() {
    if !live() {
        return;
    }
    block(async {
        let c = Client::connect(tls_opts(4225, ca())).await.unwrap();
        let s = unique("tls");
        let mut sub = c.subscribe(&s, None).unwrap();
        c.flush().await.unwrap();
        c.publish(&s, None, &[], &vec![5u8; 200_000]).unwrap();
        assert_eq!(sub.next_timeout(Duration::from_secs(5)).await.unwrap().payload.len(), 200_000);
        // The server asks for TLS even when the client did not.
        let c = Client::connect(Options { servers: vec![("localhost".into(), 4225)], ..Options::default() }).await;
        assert!(c.is_err(), "the test CA is not a system root: the certificate is refused");
        let insecure = TlsOptions { insecure: true, ..TlsOptions::default() };
        assert!(Client::connect(tls_opts(4225, insecure)).await.is_ok());
        // Wrong name: the certificate is for localhost.
        let wrong_name = Options { servers: vec![("127.0.0.1".into(), 4225)], tls: Some(ca()), ..Options::default() };
        assert!(Client::connect(wrong_name).await.is_err());
        // TLS before INFO.
        assert!(Client::connect(tls_opts(4226, TlsOptions { handshake_first: true, ..ca() })).await.is_ok());
        // Mutual TLS: with the client certificate, and without.
        let d = dir();
        let mutual = TlsOptions { cert_file: Some(d.join("client.crt")), key_file: Some(d.join("client.key")), ..ca() };
        let c = Client::connect(tls_opts(4227, mutual)).await.unwrap();
        c.flush().await.unwrap();
        let no_cert = Client::connect(tls_opts(4227, ca())).await;
        assert!(no_cert.is_err() || no_cert.as_ref().is_ok_and(|c| !c.is_connected()), "a client certificate is required");
    });
}

#[test]
fn reconnect_resubscribes_and_sends_what_waited() {
    if !live() {
        return;
    }
    block(async {
        let proxy = Proxy::to(4222);
        let c = Client::connect(Options { ping_interval: Duration::from_secs(1), ..opts(proxy.port) }).await.unwrap();
        let direct = Client::connect(opts(4222)).await.unwrap();
        let s = unique("rc");
        let mut sub = c.subscribe(&s, Some("grp")).unwrap();
        c.flush().await.unwrap();
        proxy.set_down(true);
        wait_until("the disconnect to be noticed", || !c.is_connected()).await;
        // Published while down: buffered, sent after reconnecting.
        c.publish(&s, None, &[], b"queued while down").unwrap();
        proxy.set_down(false);
        wait_until("reconnect", || c.is_connected()).await;
        assert_eq!(c.connects(), 2);
        c.flush().await.unwrap();
        assert_eq!(sub.next_timeout(Duration::from_secs(5)).await.unwrap().payload, b"queued while down");
        // The subscription was renewed on the new connection (in its queue group).
        direct.publish(&s, None, &[], b"after").unwrap();
        assert_eq!(sub.next_timeout(Duration::from_secs(5)).await.unwrap().payload, b"after");
        // A cut (no clean close) is healed too; requests work across it.
        let svc = unique("rcsvc");
        let mut rx = direct.subscribe(&svc, None).unwrap();
        direct.flush().await.unwrap();
        let d2 = direct.clone();
        rt_core::spawn(async move {
            while let Ok(m) = rx.next().await {
                let _ = d2.publish(m.reply.as_deref().unwrap(), None, &[], b"pong");
            }
        });
        proxy.cut();
        wait_until("reconnect after the cut", || c.connects() == 3 && c.is_connected()).await;
        assert_eq!(c.request(&svc, &[], b"ping", Duration::from_secs(5)).await.unwrap().payload, b"pong");
    });
}

#[test]
fn a_dead_server_is_detected_by_missing_pongs() {
    if !live() {
        return;
    }
    block(async {
        // A server that accepts and greets, then goes silent: only pings notice.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                std::thread::spawn(move || {
                    let _ = s.write_all(b"INFO {\"server_id\":\"silent\",\"max_payload\":1048576,\"headers\":true}\r\n");
                    let mut buf = [0u8; 4096];
                    let mut answered = false;
                    // Answer the handshake's PING, then never again.
                    while let Ok(n) = s.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        if !answered && buf[..n].windows(4).any(|w| w == b"PING") {
                            let _ = s.write_all(b"PONG\r\n");
                            answered = true;
                        }
                    }
                });
            }
        });
        let c = Client::connect(Options { ping_interval: Duration::from_millis(300), reconnect_for: Some(Duration::from_secs(60)), ..opts(port) }).await.unwrap();
        assert!(c.is_connected());
        wait_until("the stale connection to be dropped", || !c.is_connected() || c.connects() > 1).await;
    });
}

#[test]
fn cluster_urls_are_learned_and_used_for_failover() {
    if !live() {
        return;
    }
    block(async {
        let proxy = Proxy::to(4230);
        let c = Client::connect(Options { ping_interval: Duration::from_secs(1), ..opts(proxy.port) }).await.unwrap();
        wait_until("the cluster's URLs", || c.server_info()["connect_urls"].as_array().is_some_and(|u| u.len() >= 2)).await;
        let urls: Vec<String> = c.server_info()["connect_urls"].as_array().unwrap().iter().map(|u| u.as_str().unwrap().to_string()).collect();
        assert!(urls.iter().any(|u| u.ends_with(":4231")) && urls.iter().any(|u| u.ends_with(":4232")), "{urls:?}");
        let s = unique("fo");
        let mut sub = c.subscribe(&s, None).unwrap();
        c.flush().await.unwrap();
        // The configured address dies for good: the client moves on to a
        // member it learned from the cluster (possibly the same node, at the
        // address that node advertises) — not through the dead proxy.
        proxy.set_down(true);
        wait_until("failover", || c.is_connected() && c.connects() == 2).await;
        assert!(proxy.conns.lock().unwrap().is_empty(), "connected without the configured address");
        let other = Client::connect(opts(4232)).await.unwrap();
        other.flush().await.unwrap();
        // Interest propagates over the routes.
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            other.publish(&s, None, &[], b"routed").unwrap();
            match sub.next_timeout(Duration::from_millis(300)).await {
                Ok(m) => {
                    assert_eq!(m.payload, b"routed");
                    break;
                }
                Err(_) => assert!(Instant::now() < until, "no message over the cluster"),
            }
        }
    });
}

#[test]
fn jetstream_dedup_purge_info_and_direct_reads() {
    if !live() {
        return;
    }
    block(async {
        let b = NatsBroker::connect(opts(4222)).await.unwrap();
        let js = b.jetstream().clone();
        assert!(js.account_info().await.unwrap()["limits"].is_object());
        let topic = unique("js");
        b.ensure_topic(&topic).await.unwrap();
        b.ensure_topic(&topic).await.unwrap();
        let stream = kernway_nats::jetstream::stream_name(&topic);
        let (st, s1, dup) = js.publish(&topic, &[], b"one", Some("id-1")).await.unwrap();
        assert_eq!((st.as_str(), s1, dup), (stream.as_str(), 1, false));
        let (_, s2, dup) = js.publish(&topic, &[], b"one again", Some("id-1")).await.unwrap();
        assert_eq!((s2, dup), (1, true), "same Nats-Msg-Id: dropped as a duplicate");
        js.publish(&topic, &[("H".into(), "v".into())], b"two", Some("id-2")).await.unwrap();
        let info = js.stream_info(&stream).await.unwrap();
        assert_eq!(info["state"]["messages"], 2);
        let last = js.last_message(&stream, &topic).await.unwrap().unwrap();
        assert_eq!(last["seq"], 2);
        assert_eq!(js.purge_stream(&stream).await.unwrap(), 2);
        assert!(js.last_message(&stream, &topic).await.unwrap().is_none());
        // Publishing where no stream listens is a clear error, not a hang.
        let err = b.producer().await.unwrap().publish(&unique("nostream"), OutMessage::new("x")).await.unwrap_err();
        assert!(matches!(&err, MsgError::Broker(m) if m.contains("no stream")), "{err}");
        b.delete_topic(&topic).await.unwrap();
        b.delete_topic(&topic).await.unwrap();
        assert!(js.stream_info(&stream).await.is_err());
    });
}

#[test]
fn consumers_survive_a_reconnect() {
    if !live() {
        return;
    }
    block(async {
        let proxy = Proxy::to(4222);
        let b = NatsBroker::connect(Options { ping_interval: Duration::from_secs(1), ..opts(proxy.port) }).await.unwrap();
        let topic = unique("jsrc");
        b.ensure_topic(&topic).await.unwrap();
        let p = b.producer().await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        p.publish(&topic, OutMessage::new("before")).await.unwrap();
        let d = c.next_timeout(Duration::from_secs(5)).await.unwrap();
        c.ack(&d).await.unwrap();
        proxy.cut();
        let client = b.jetstream().client().clone();
        wait_until("reconnect", || client.connects() == 2 && client.is_connected()).await;
        p.publish(&topic, OutMessage::new("after").key("k")).await.unwrap();
        let d = c.next_timeout(Duration::from_secs(10)).await.unwrap();
        assert_eq!((d.payload.as_slice(), d.key.as_deref()), (&b"after"[..], Some(&b"k"[..])));
        assert!(!d.redelivered);
        c.ack(&d).await.unwrap();
        assert!(matches!(c.next_timeout(Duration::from_millis(1500)).await, Err(MsgError::Timeout)), "`before` stays acknowledged");
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

#[test]
fn the_cluster_runs_the_conformance_scenarios_too() {
    if !live() {
        return;
    }
    // JetStream replicated over three nodes (R3) — the contract holds there.
    #[derive(Clone)]
    struct Cluster;
    impl kernway_messaging::conformance::Harness for Cluster {
        type Broker = NatsBroker;
        async fn broker(&self) -> NatsBroker {
            let mut o = opts(4230);
            o.servers.push(("127.0.0.1".into(), 4231));
            let mut b = NatsBroker::connect(o).await.unwrap();
            b.replicas = 3;
            b
        }
    }
    kernway_messaging::conformance::run_all(Cluster);
}
