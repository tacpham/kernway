//! The AMQP client against a live RabbitMQ — set up by
//! `tests/support/rabbitmq-setup.sh`, which prints `KERNWAY_RABBITMQ_URL`,
//! `KERNWAY_RABBITMQ_DIR` (certificates) and `KERNWAY_RABBITMQ_CTL` (a
//! rabbitmqctl command). Without them each test reports `skip:`, which the
//! CI job and the acceptance run treat as a failure.

use kernway_messaging::{Broker, Consumer, MsgError, OutMessage, Producer, Subscription};
use kernway_rabbitmq::{Connection, Field, Mechanism, Options, Properties, RabbitBroker, TlsOptions};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn live() -> bool {
    let up = std::env::var_os("KERNWAY_RABBITMQ_URL").is_some();
    if !up {
        eprintln!("skip: KERNWAY_RABBITMQ_URL not set (tests/support/rabbitmq-setup.sh)");
    }
    up
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn url() -> String {
    std::env::var("KERNWAY_RABBITMQ_URL").unwrap()
}

fn opts() -> Options {
    Options::parse(&url()).unwrap()
}

fn dir() -> PathBuf {
    PathBuf::from(std::env::var("KERNWAY_RABBITMQ_DIR").expect("KERNWAY_RABBITMQ_DIR"))
}

/// Run `rabbitmqctl <args>` against the test broker.
fn ctl(args: &str) -> String {
    let cmd = std::env::var("KERNWAY_RABBITMQ_CTL").expect("KERNWAY_RABBITMQ_CTL");
    let shell = std::env::var("KERNWAY_RABBITMQ_SHELL").unwrap_or_else(|_| "bash".into());
    let out = std::process::Command::new(&shell).arg("-c").arg(format!("{cmd} {args}")).output().expect("run rabbitmqctl");
    assert!(
        out.status.success(),
        "rabbitmqctl {args} (via {shell}): {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn unique(name: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64;
    format!("kwt-{name}-{}-{:x}-{}", std::process::id(), t & 0xff_ffff, N.fetch_add(1, Ordering::Relaxed))
}

async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        rt_core::sleep(Duration::from_millis(20)).await;
    }
}

/// A TCP proxy to the broker: connections can be cut, or frozen (kept open,
/// nothing forwarded — a silent peer).
struct Proxy {
    port: u16,
    frozen: Arc<AtomicBool>,
    conns: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    fn to(target: u16) -> Proxy {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let frozen = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let (fz, c) = (frozen.clone(), conns.clone());
        std::thread::spawn(move || {
            for client in l.incoming() {
                let Ok(client) = client else { continue };
                let Ok(server) = TcpStream::connect(("127.0.0.1", target)) else { continue };
                c.lock().unwrap().extend([client.try_clone().unwrap(), server.try_clone().unwrap()]);
                for (mut from, mut to) in [(client.try_clone().unwrap(), server.try_clone().unwrap()), (server, client)] {
                    let fz = fz.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 65536];
                        while let Ok(n) = from.read(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            if fz.load(Ordering::SeqCst) {
                                continue; // swallowed
                            }
                            if to.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                        let _ = to.shutdown(Shutdown::Both);
                        let _ = from.shutdown(Shutdown::Both);
                    });
                }
            }
        });
        Proxy { port, frozen, conns }
    }

    fn cut(&self) {
        for s in self.conns.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

#[test]
fn handshake_tuning_and_server_properties() {
    if !live() {
        return;
    }
    block(async {
        let mut o = opts();
        o.heartbeat = Duration::from_secs(5);
        o.frame_max = 65_536;
        o.name = Some("kw-test".into());
        let c = Connection::connect(o).await.unwrap();
        let props = c.server_properties();
        let product = props.iter().find(|(k, _)| k == "product").and_then(|(_, v)| v.as_text());
        assert_eq!(product.as_deref(), Some("RabbitMQ"));
        let (channel_max, frame_max, hb) = c.tuning();
        assert_eq!((frame_max, hb), (65_536, Duration::from_secs(5)), "the lower of ours and the broker's");
        assert!(channel_max > 0);
        // Many channels on one connection.
        let mut chans = Vec::new();
        for _ in 0..20 {
            chans.push(c.channel().await.unwrap());
        }
        let ids: std::collections::HashSet<u16> = chans.iter().map(|c| c.id()).collect();
        assert_eq!(ids.len(), 20);
        for ch in chans {
            ch.close().await.unwrap();
        }
        // Idle past several heartbeats: still open (both sides kept it alive).
        rt_core::sleep(Duration::from_secs(12)).await;
        assert!(c.is_open());
        let ch = c.channel().await.unwrap();
        ch.queue_declare("", false, true, true, &Vec::new()).await.unwrap();
        c.close().await.unwrap();
        assert!(!c.is_open());
    });
}

#[test]
fn authentication_and_virtual_hosts() {
    if !live() {
        return;
    }
    block(async {
        let with = |user: &str, pass: &str, vhost: &str, m: Mechanism| Options { user: user.into(), password: pass.into(), vhost: vhost.into(), mechanism: m, ..opts() };
        assert!(Connection::connect(with("kw", "kw_pw", "/", Mechanism::Plain)).await.is_ok());
        assert!(Connection::connect(with("kw", "kw_pw", "kw-vhost", Mechanism::AmqPlain)).await.is_ok(), "AMQPLAIN, another vhost");
        let e = Connection::connect(with("kw", "wrong", "/", Mechanism::Plain)).await.err().unwrap();
        assert!(matches!(e, MsgError::Auth(_)), "{e}");
        let e = Connection::connect(with("kw-noaccess", "na_pw", "/", Mechanism::Plain)).await.err().unwrap();
        assert!(matches!(&e, MsgError::Auth(m) if m.contains("530")), "no vhost access: {e}");
        let e = Connection::connect(with("kw", "kw_pw", "no-such-vhost", Mechanism::Plain)).await.err().unwrap();
        assert!(matches!(e, MsgError::Auth(_)), "{e}");
        // The URI form, vhost percent-encoded.
        let u = url().replace("%2F", "kw-vhost");
        let b = RabbitBroker::connect_uri(&u).await.unwrap();
        let t = unique("vh");
        b.ensure_topic(&t).await.unwrap();
        b.delete_topic(&t).await.unwrap();
    });
}

#[test]
fn tls_mutual_tls_and_external_auth() {
    if !live() {
        return;
    }
    block(async {
        let d = dir();
        let ca = TlsOptions { ca_file: Some(d.join("ca.pem")), ..TlsOptions::default() };
        let tls = |t: TlsOptions, host: &str| Options { servers: vec![(host.into(), 5671)], tls: Some(t), ..opts() };
        let c = Connection::connect(tls(ca.clone(), "localhost")).await.unwrap();
        let ch = c.channel().await.unwrap();
        // A large body through TLS records.
        let (q, _, _) = ch.queue_declare("", false, true, true, &Vec::new()).await.unwrap();
        ch.publish("", &q, false, &Properties::default(), &vec![9u8; 3 << 20]).unwrap();
        let got = loop {
            if let Some(m) = ch.get(&q, true).await.unwrap() {
                break m;
            }
            rt_core::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(got.body.len(), 3 << 20);
        // The certificate is for localhost, not 127.0.0.1; the CA is not a system root.
        assert!(Connection::connect(tls(ca.clone(), "127.0.0.1")).await.is_err());
        assert!(Connection::connect(tls(TlsOptions::default(), "localhost")).await.is_err());
        assert!(Connection::connect(tls(TlsOptions { insecure: true, ..TlsOptions::default() }, "127.0.0.1")).await.is_ok());
        // Mutual TLS + EXTERNAL: the certificate's CN is the user.
        let mutual = TlsOptions { cert_file: Some(d.join("client.crt")), key_file: Some(d.join("client.key")), ..ca.clone() };
        let ext = Options { mechanism: Mechanism::External, user: String::new(), password: String::new(), ..tls(mutual.clone(), "localhost") };
        let c = Connection::connect(ext).await.unwrap();
        c.channel().await.unwrap().queue_declare("", false, true, true, &Vec::new()).await.unwrap();
        // EXTERNAL without a certificate has no identity.
        let ext_no_cert = Options { mechanism: Mechanism::External, ..tls(ca.clone(), "localhost") };
        assert!(matches!(Connection::connect(ext_no_cert).await, Err(MsgError::Auth(_))));
        // The URI form.
        let p = |f: &str| d.join(f).to_string_lossy().replace('\\', "/");
        let u = format!("amqps://localhost/%2F?cacertfile={}&certfile={}&keyfile={}&auth_mechanism=external", p("ca.pem"), p("client.crt"), p("client.key"));
        assert!(Connection::connect_uri(&u).await.is_ok());
    });
}

#[test]
fn exchanges_queues_bindings_routing_and_get() {
    if !live() {
        return;
    }
    block(async {
        let c = Connection::connect(opts()).await.unwrap();
        let ch = c.channel().await.unwrap();
        let ex = unique("topic-x");
        ch.exchange_declare(&ex, "topic", false, true, &Vec::new()).await.unwrap();
        ch.exchange_declare(&ex, "topic", false, true, &Vec::new()).await.unwrap();
        let (q1, msgs, consumers) = ch.queue_declare("", false, true, true, &Vec::new()).await.unwrap();
        assert!(q1.starts_with("amq.gen-"), "server-named: {q1}");
        assert_eq!((msgs, consumers), (0, 0));
        let (q2, _, _) = ch.queue_declare("", false, true, true, &Vec::new()).await.unwrap();
        ch.queue_bind(&q1, &ex, "pos.*.checkin", &Vec::new()).await.unwrap();
        ch.queue_bind(&q2, &ex, "pos.#", &Vec::new()).await.unwrap();
        ch.confirm_select().await.unwrap();
        for rk in ["pos.42.checkin", "pos.42.checkout", "other.x"] {
            let s = ch.publish(&ex, rk, false, &Properties::default(), rk.as_bytes()).unwrap();
            assert!(ch.wait_confirm(s).await.unwrap());
        }
        assert_eq!(ch.queue_inspect(&q1).await.unwrap().0, 1);
        assert_eq!(ch.queue_inspect(&q2).await.unwrap().0, 2);
        let m = ch.get(&q1, false).await.unwrap().unwrap();
        assert_eq!((m.body.as_slice(), m.routing_key.as_str(), m.exchange.as_str()), (&b"pos.42.checkin"[..], "pos.42.checkin", ex.as_str()));
        ch.ack(m.delivery_tag, false).unwrap();
        assert!(ch.get(&q1, false).await.unwrap().is_none(), "Get-Empty");
        ch.queue_unbind(&q2, &ex, "pos.#", &Vec::new()).await.unwrap();
        let s = ch.publish(&ex, "pos.1.checkin", false, &Properties::default(), b"x").unwrap();
        ch.wait_confirm(s).await.unwrap();
        assert_eq!(ch.queue_purge(&q2).await.unwrap(), 2, "unbound: nothing new");
        assert_eq!(ch.queue_delete(&q1, false, false).await.unwrap(), 1);
        // A failed passive declare closes that channel only.
        let probe = c.channel().await.unwrap();
        let e = probe.queue_inspect(&unique("missing")).await.unwrap_err();
        assert!(matches!(&e, MsgError::Broker(m) if m.contains("404")), "{e}");
        assert!(!probe.is_open() && ch.is_open() && c.is_open());
        let e = c.channel().await.unwrap().exchange_exists(&unique("nox")).await.unwrap_err();
        assert!(matches!(&e, MsgError::Broker(m) if m.contains("404")), "{e}");
        ch.exchange_delete(&ex, false).await.unwrap();
    });
}

#[test]
fn properties_headers_and_large_bodies_round_trip() {
    if !live() {
        return;
    }
    block(async {
        // RabbitMQ 4's smallest frame: a large body goes in many frames.
        let c = Connection::connect(Options { frame_max: 8192, ..opts() }).await.unwrap();
        let ch = c.channel().await.unwrap();
        let (q, _, _) = ch.queue_declare("", false, true, true, &Vec::new()).await.unwrap();
        let headers = vec![
            ("s".to_string(), Field::str("Việt ✓")),
            ("n".to_string(), Field::I64(-42)),
            ("b".to_string(), Field::Bool(true)),
            ("f".to_string(), Field::F64(1.25)),
            ("bin".to_string(), Field::Bytes(vec![0, 1, 255])),
            ("arr".to_string(), Field::Array(vec![Field::I32(1), Field::str("two")])),
            ("tbl".to_string(), Field::Table(vec![("in".into(), Field::str("side"))])),
            ("ts".to_string(), Field::Timestamp(1_700_000_000)),
            ("dec".to_string(), Field::Decimal(2, 1999)),
        ];
        let props = Properties {
            content_type: Some("application/json".into()),
            content_encoding: Some("gzip".into()),
            headers: Some(headers),
            delivery_mode: Some(2),
            priority: Some(5),
            correlation_id: Some("corr".into()),
            reply_to: Some("replies".into()),
            expiration: Some("60000".into()),
            message_id: Some("mid".into()),
            timestamp: Some(1_700_000_001),
            kind: Some("checkin".into()),
            user_id: Some("kw".into()),
            app_id: Some("joy-pos".into()),
        };
        // 2 MiB over 8 KiB frames: 250+ body frames.
        let body: Vec<u8> = (0..2 << 20).map(|i| (i % 253) as u8).collect();
        ch.publish("", &q, false, &props, &body).unwrap();
        ch.publish("", &q, false, &Properties::default(), b"").unwrap();
        let tag = ch.consume(&q, "kw-tag", false, false, &Vec::new()).await.unwrap();
        assert_eq!(tag, "kw-tag");
        let m = ch.next_delivery(Some(Duration::from_secs(10))).await.unwrap().unwrap();
        assert_eq!(m.body, body);
        // Every field survives; RabbitMQ keeps header tables sorted by name.
        let sorted = |p: &Properties| {
            let mut p = p.clone();
            if let Some(h) = p.headers.as_mut() {
                h.sort_by(|a, b| a.0.cmp(&b.0));
            }
            p
        };
        assert_eq!(sorted(&m.props), sorted(&props));
        let empty = ch.next_delivery(Some(Duration::from_secs(5))).await.unwrap().unwrap();
        assert!(empty.body.is_empty());
        ch.ack(empty.delivery_tag, true).unwrap();
        // A user id that is not the connection's user is refused.
        let bad = Properties { user_id: Some("someone-else".into()), ..Properties::default() };
        let ch2 = c.channel().await.unwrap();
        ch2.confirm_select().await.unwrap();
        let s = ch2.publish("", &q, false, &bad, b"x").unwrap();
        let e = ch2.wait_confirm(s).await.unwrap_err();
        assert!(matches!(&e, MsgError::Broker(m) if m.contains("406")), "{e}");
    });
}

#[test]
fn confirms_returns_and_a_missing_topic() {
    if !live() {
        return;
    }
    block(async {
        let b = RabbitBroker::connect(opts()).await.unwrap();
        let p = b.producer().await.unwrap();
        let topic = unique("ret");
        b.ensure_topic(&topic).await.unwrap();
        // No group yet: the message has nowhere to go, and the receipt says so.
        let r = p.publish(&topic, OutMessage::new("lost")).await.unwrap();
        assert!(r.id.starts_with("unrouted"), "{r:?}");
        let mut c = b.subscribe(Subscription::new(&topic, "g")).await.unwrap();
        let r = p.publish(&topic, OutMessage::new("kept")).await.unwrap();
        assert!(r.id.parse::<u64>().is_ok(), "a confirm sequence: {r:?}");
        let d = c.next_timeout(Duration::from_secs(5)).await.unwrap();
        assert_eq!(d.payload, b"kept");
        c.ack(&d).await.unwrap();
        // A topic nobody created: an error, and the producer recovers after it.
        let e = p.publish(&unique("nowhere"), OutMessage::new("x")).await.unwrap_err();
        assert!(matches!(&e, MsgError::Broker(m) if m.contains("404")), "{e}");
        p.publish(&topic, OutMessage::new("after")).await.unwrap();
        let d = c.next_timeout(Duration::from_secs(5)).await.unwrap();
        assert_eq!(d.payload, b"after");
        c.ack(&d).await.unwrap();
        // Many publishes in flight on one channel, each confirmed.
        let ch = b.connection().await.unwrap().channel().await.unwrap();
        ch.confirm_select().await.unwrap();
        let seqs: Vec<u64> = (0..500).map(|i| ch.publish(&topic, "", false, &Properties::default(), format!("{i}").as_bytes()).unwrap()).collect();
        assert_eq!(seqs, (1..=500).collect::<Vec<_>>());
        for s in seqs.iter().rev() {
            assert!(ch.wait_confirm(*s).await.unwrap(), "confirm {s}");
        }
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

#[test]
fn prefetch_limits_what_a_consumer_holds() {
    if !live() {
        return;
    }
    block(async {
        let c = Connection::connect(opts()).await.unwrap();
        let ch = c.channel().await.unwrap();
        let (q, _, _) = ch.queue_declare("", false, true, true, &Vec::new()).await.unwrap();
        for i in 0..10 {
            ch.publish("", &q, false, &Properties::default(), format!("{i}").as_bytes()).unwrap();
        }
        let consumer = c.channel().await.unwrap();
        consumer.qos(3).await.unwrap();
        consumer.consume(&q, "", false, false, &Vec::new()).await.unwrap();
        let mut held = Vec::new();
        while let Some(d) = consumer.next_delivery(Some(Duration::from_millis(700))).await.unwrap() {
            held.push(d);
        }
        assert_eq!(held.len(), 3, "prefetch 3");
        consumer.ack(held[2].delivery_tag, true).unwrap();
        let mut more = 0;
        while consumer.next_delivery(Some(Duration::from_millis(700))).await.unwrap().is_some() {
            more += 1;
        }
        assert_eq!(more, 3, "three more after acknowledging three");
        // Rejected without requeue (no dead-letter exchange): dropped.
        let (q2, _, _) = ch.queue_declare("", false, true, true, &Vec::new()).await.unwrap();
        ch.publish("", &q2, false, &Properties::default(), b"bad").unwrap();
        let m = loop {
            if let Some(m) = ch.get(&q2, false).await.unwrap() {
                break m;
            }
        };
        ch.nack(m.delivery_tag, false, false).unwrap();
        rt_core::sleep(Duration::from_millis(300)).await;
        assert!(ch.get(&q2, false).await.unwrap().is_none());
    });
}

#[test]
fn a_silent_broker_is_detected_by_heartbeats() {
    if !live() {
        return;
    }
    block(async {
        let proxy = Proxy::to(5672);
        let o = Options { servers: vec![("127.0.0.1".into(), proxy.port)], heartbeat: Duration::from_secs(1), ..opts() };
        let c = Connection::connect(o).await.unwrap();
        rt_core::sleep(Duration::from_secs(3)).await;
        assert!(c.is_open(), "heartbeats keep an idle connection");
        proxy.frozen.store(true, Ordering::SeqCst);
        let started = Instant::now();
        wait_until("the dead connection to be noticed", || !c.is_open()).await;
        assert!(started.elapsed() < Duration::from_secs(6), "two missed heartbeats, not a TCP timeout");
        assert!(matches!(c.channel().await, Err(MsgError::Connection(_))));
    });
}

#[test]
fn consumers_and_producers_survive_a_lost_connection() {
    if !live() {
        return;
    }
    block(async {
        let proxy = Proxy::to(5672);
        let b = RabbitBroker::connect(Options { servers: vec![("127.0.0.1".into(), proxy.port)], heartbeat: Duration::from_secs(2), ..opts() }).await.unwrap();
        let topic = unique("lost");
        let p = b.producer().await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g").max_in_flight(10)).await.unwrap();
        for i in 0..3 {
            p.publish(&topic, OutMessage::new(format!("m{i}")).key("k")).await.unwrap();
        }
        let first = c.next_timeout(Duration::from_secs(5)).await.unwrap();
        c.ack(&first).await.unwrap();
        let held = c.next_timeout(Duration::from_secs(5)).await.unwrap();
        assert_eq!(held.payload, b"m1");
        // The ack of m0 is on the wire once `ack` returns; give the network
        // (the proxy) time to deliver it, then lose the connection.
        rt_core::sleep(Duration::from_millis(300)).await;
        proxy.cut();
        wait_until("the lost connection to be noticed", || !b.is_connected()).await;
        // The unacknowledged ones come again on the new connection (m1
        // flagged as redelivered); m0, acknowledged before the cut, does not.
        let mut got = std::collections::BTreeMap::new();
        while got.len() < 2 {
            let d = c.next_timeout(Duration::from_secs(15)).await.unwrap();
            got.insert(String::from_utf8(d.payload.clone()).unwrap(), d.redelivered);
            c.ack(&d).await.unwrap();
        }
        assert!(!got.contains_key("m0"), "acknowledged before the cut: {got:?}");
        assert_eq!(got.get("m1"), Some(&true), "{got:?}");
        assert!(got.contains_key("m2"), "{got:?}");
        match c.next_timeout(Duration::from_secs(2)).await {
            Err(MsgError::Timeout) => {}
            Ok(d) => panic!("nothing more expected, got {:?} (redelivered {}) after {got:?}", String::from_utf8_lossy(&d.payload), d.redelivered),
            Err(e) => panic!("{e}"),
        }
        // Acknowledging the delivery from the lost channel is harmless.
        c.ack(&held).await.unwrap();
        assert!(b.connects() >= 2);
        p.publish(&topic, OutMessage::new("after")).await.unwrap();
        let d = c.next_timeout(Duration::from_secs(5)).await.unwrap();
        assert_eq!(d.payload, b"after");
        c.ack(&d).await.unwrap();
        // The broker closing every connection (an operator, a restart): the
        // publish that meets the closed connection may fail (its outcome is
        // unknown — at-least-once means the caller retries); a retry reconnects.
        let before = b.connects();
        ctl("close_all_connections kw-test");
        let mut sent = false;
        for _ in 0..50 {
            if p.publish(&topic, OutMessage::new("again")).await.is_ok() && b.connects() > before {
                sent = true;
                break;
            }
            rt_core::sleep(Duration::from_millis(200)).await;
        }
        assert!(sent, "published on a new connection");
        let d = c.next_timeout(Duration::from_secs(15)).await.unwrap();
        assert_eq!(d.payload, b"again", "the consumer reconnected too");
        c.ack(&d).await.unwrap();
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

#[test]
fn the_broker_cancels_a_consumer_whose_queue_is_deleted() {
    if !live() {
        return;
    }
    block(async {
        let b = RabbitBroker::connect(opts()).await.unwrap();
        let topic = unique("cancel");
        let mut c = b.subscribe(Subscription::new(&topic, "g")).await.unwrap();
        let ch = b.connection().await.unwrap().channel().await.unwrap();
        ch.queue_delete(&RabbitBroker::queue_name(&topic, "g"), false, false).await.unwrap();
        let e = c.next_timeout(Duration::from_secs(10)).await.unwrap_err();
        assert!(matches!(&e, MsgError::Broker(m) if m.contains("cancel") || m.contains("404")), "{e}");
        b.delete_topic(&topic).await.unwrap();
    });
}

#[test]
fn memory_alarms_block_publishers() {
    if !live() {
        return;
    }
    block(async {
        let c = Connection::connect(opts()).await.unwrap();
        let ch = c.channel().await.unwrap();
        let (q, _, _) = ch.queue_declare("", false, true, true, &Vec::new()).await.unwrap();
        ch.publish("", &q, false, &Properties::default(), b"x").unwrap();
        ctl("set_vm_memory_high_watermark 0.0000001");
        let r = async {
            wait_until("Connection.Blocked", || c.blocked().is_some()).await;
        };
        // Always lift the alarm, whatever happened.
        let blocked = rt_core::timeout(Duration::from_secs(20), r).await.is_ok();
        ctl("set_vm_memory_high_watermark 0.4");
        assert!(blocked, "the broker told the client it is blocked");
        wait_until("Connection.Unblocked", || c.blocked().is_none()).await;
    });
}

#[test]
fn refused_tuning_is_explained() {
    if !live() {
        return;
    }
    block(async {
        // Within the protocol's minimum (4096), below RabbitMQ 4's (8192).
        let e = Connection::connect(Options { frame_max: 4096, ..opts() }).await.err().unwrap();
        assert!(matches!(&e, MsgError::Connection(m) if m.contains("frame_max 4096") && m.contains("8192")), "{e}");
    });
}
