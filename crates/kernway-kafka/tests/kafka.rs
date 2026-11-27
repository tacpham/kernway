//! The Kafka client against a live broker — set up by
//! `tests/support/kafka-setup.sh`, which prints `KERNWAY_KAFKA_URL`,
//! `KERNWAY_KAFKA_DIR` (certificates), `KERNWAY_KAFKA_TOOLS` (the Kafka
//! command-line tools) and `KERNWAY_KAFKA_RESTART`. Without them each test
//! reports `skip:`, which the CI job and the acceptance run treat as a failure.

use kernway_kafka::client::{partition_for, Client};
use kernway_kafka::records::{ProducerIds, Record};
use kernway_kafka::{Broker, Consumer, KafkaBroker, MsgError, Options, OutMessage, Producer, Subscription};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

fn live() -> bool {
    let up = std::env::var_os("KERNWAY_KAFKA_DIR").is_some();
    if !up {
        eprintln!("skip: KERNWAY_KAFKA_DIR not set (tests/support/kafka-setup.sh)");
    }
    up
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn dir() -> PathBuf {
    PathBuf::from(std::env::var("KERNWAY_KAFKA_DIR").unwrap())
}

fn path(f: &str) -> String {
    dir().join(f).to_string_lossy().replace('\\', "/")
}

fn unique(name: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64;
    format!("kwt-{name}-{}-{:x}-{}", std::process::id(), t & 0xff_ffff, N.fetch_add(1, Ordering::Relaxed))
}

/// Run a shell command line (the Kafka tools, a restart).
fn sh(line: &str) -> String {
    let shell = std::env::var("KERNWAY_KAFKA_SHELL").unwrap_or_else(|_| "bash".into());
    let out = std::process::Command::new(&shell).arg("-c").arg(line).output().expect("run a command");
    assert!(
        out.status.success(),
        "{line} (via {shell}): {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn tool(name: &str) -> String {
    format!("{}/{name}{}", std::env::var("KERNWAY_KAFKA_TOOLS").unwrap(), std::env::var("KERNWAY_KAFKA_TOOL_EXT").unwrap())
}

async fn round_trip(c: &Client, what: &str) {
    let t = unique("rt");
    c.create_topic(&t, 1, 1).await.unwrap_or_else(|e| panic!("{what}: {e}"));
    let rec = Record { timestamp: 1, key: Some(b"k".to_vec()), value: Some(what.as_bytes().to_vec()), ..Record::default() };
    assert_eq!(c.produce(&t, 0, rec).await.unwrap(), 0, "{what}");
    let f = c.fetch(&t, 0, 0, Duration::from_millis(500), 1 << 20).await.unwrap();
    assert_eq!(f.records[0].value.as_deref(), Some(what.as_bytes()), "{what}");
    c.delete_topic(&t).await.unwrap();
}

#[test]
fn sasl_plain_and_scram() {
    if !live() {
        return;
    }
    block(async {
        for mech in ["plain", "scram-sha-256", "scram-sha-512"] {
            let c = Client::connect(Options::parse(&format!("kafka://kw:kw_pw@127.0.0.1:9093?sasl={mech}")).unwrap()).await.unwrap_or_else(|e| panic!("{mech}: {e}"));
            round_trip(&c, mech).await;
            let bad = Client::connect(Options::parse(&format!("kafka://kw:wrong@127.0.0.1:9093?sasl={mech}")).unwrap()).await;
            assert!(matches!(bad, Err(MsgError::Auth(_))), "{mech}, wrong password: {:?}", bad.err());
        }
        // The SASL listener without credentials: refused, not hung.
        let started = Instant::now();
        assert!(Client::connect(Options { request_timeout: Duration::from_secs(5), ..Options::parse("kafka://127.0.0.1:9093").unwrap() }).await.is_err());
        assert!(started.elapsed() < Duration::from_secs(15));
    });
}

#[test]
fn tls_server_check_and_client_certificate() {
    if !live() {
        return;
    }
    block(async {
        let ca = path("ca.pem");
        let c = Client::connect(Options::parse(&format!("kafkas://localhost:9094?ca={ca}")).unwrap()).await.unwrap();
        round_trip(&c, "tls").await;
        let mutual = Client::connect(Options::parse(&format!("kafkas://localhost:9094?ca={ca}&cert={}&key={}", path("client.crt"), path("client.key"))).unwrap()).await.unwrap();
        round_trip(&mutual, "mutual tls").await;
        // The certificate names localhost; the test CA is not a system root.
        assert!(Client::connect(Options::parse(&format!("kafkas://127.0.0.1:9094?ca={ca}")).unwrap()).await.is_err());
        assert!(Client::connect(Options::parse("kafkas://localhost:9094").unwrap()).await.is_err());
        assert!(Client::connect(Options::parse("kafkas://localhost:9094?insecure=true").unwrap()).await.is_ok());
    });
}

#[test]
fn the_broker_drops_duplicate_batches() {
    if !live() {
        return;
    }
    block(async {
        let c = Client::connect(Options::parse(&std::env::var("KERNWAY_KAFKA_URL").unwrap()).unwrap()).await.unwrap();
        let t = unique("dup");
        c.create_topic(&t, 1, 1).await.unwrap();
        let (id, epoch) = c.producer_ids().await.unwrap();
        let rec = |v: &str| vec![Record { timestamp: 1, value: Some(v.as_bytes().to_vec()), ..Record::default() }];
        let ids = |seq| ProducerIds { id, epoch, base_sequence: seq };
        assert_eq!(c.produce_raw(&t, 0, &rec("a"), ids(0)).await.unwrap(), (0, 0));
        // The same batch again (a retry whose first answer was lost): no second copy.
        let (code, off) = c.produce_raw(&t, 0, &rec("a"), ids(0)).await.unwrap();
        assert!(code == 0 || code == 46, "duplicate answered with {code}");
        assert_eq!(off, 0, "the original offset");
        assert_eq!(c.produce_raw(&t, 0, &rec("b"), ids(1)).await.unwrap(), (0, 1));
        // A gap in the sequence is refused.
        assert_eq!(c.produce_raw(&t, 0, &rec("c"), ids(5)).await.unwrap().0, 45, "OUT_OF_ORDER_SEQUENCE_NUMBER");
        let f = c.fetch(&t, 0, 0, Duration::from_millis(500), 1 << 20).await.unwrap();
        let vals: Vec<_> = f.records.iter().map(|r| String::from_utf8(r.value.clone().unwrap()).unwrap()).collect();
        assert_eq!(vals, ["a", "b"]);
        // Another producer (its own id and sequences) appends normally.
        let other = Client::connect(Options::parse(&std::env::var("KERNWAY_KAFKA_URL").unwrap()).unwrap()).await.unwrap();
        let r = Record { timestamp: 1, value: Some(b"d".to_vec()), ..Record::default() };
        assert_eq!(other.produce(&t, 0, r).await.unwrap(), 2);
        c.delete_topic(&t).await.unwrap();
    });
}

#[test]
fn reads_what_the_java_client_writes() {
    if !live() {
        return;
    }
    block(async {
        let c = Client::connect(Options::parse(&std::env::var("KERNWAY_KAFKA_URL").unwrap()).unwrap()).await.unwrap();
        for codec in ["gzip", "snappy", "none"] {
            let t = unique(&format!("java-{codec}"));
            c.create_topic(&t, 3, 1).await.unwrap();
            let keys: Vec<String> = (0..30).map(|i| format!("key-{i}")).collect();
            let input: String = keys.iter().enumerate().map(|(i, k)| format!("{k}:value {i} Việt ✓\n")).collect();
            let cmd = format!(
                "printf '%s' '{input}' | {} --bootstrap-server 127.0.0.1:9092 --topic {t} --compression-codec {codec} --property parse.key=true --property key.separator=:",
                tool("kafka-console-producer")
            );
            sh(&cmd);
            let mut got = BTreeMap::new();
            for p in 0..3 {
                let f = c.fetch(&t, p, 0, Duration::from_millis(500), 1 << 20).await.unwrap();
                for r in f.records {
                    let k = String::from_utf8(r.key.unwrap()).unwrap();
                    // Our partitioner puts a key where the Java client does.
                    assert_eq!(partition_for(k.as_bytes(), 3), p, "{codec}: key {k}");
                    got.insert(k, String::from_utf8(r.value.unwrap()).unwrap());
                }
            }
            assert_eq!(got.len(), 30, "{codec}: every record");
            assert_eq!(got["key-7"], "value 7 Việt ✓");
            c.delete_topic(&t).await.unwrap();
        }
        // lz4 batches: a clear error, not garbage.
        let t = unique("java-lz4");
        c.create_topic(&t, 1, 1).await.unwrap();
        sh(&format!("printf 'x\\n' | {} --bootstrap-server 127.0.0.1:9092 --topic {t} --compression-codec lz4", tool("kafka-console-producer")));
        let e = c.fetch(&t, 0, 0, Duration::from_millis(500), 1 << 20).await.unwrap_err();
        assert!(e.to_string().contains("lz4"), "{e}");
        c.delete_topic(&t).await.unwrap();
    });
}

#[test]
fn producers_and_consumers_survive_a_broker_restart() {
    if !live() {
        return;
    }
    block(async {
        let b = KafkaBroker::connect_url(&std::env::var("KERNWAY_KAFKA_URL").unwrap()).await.unwrap();
        let topic = unique("restart");
        b.ensure_topic(&topic).await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        let p = b.producer().await.unwrap();
        p.publish(&topic, OutMessage::new("before").key("k")).await.unwrap();
        let d = c.next_timeout(Duration::from_secs(10)).await.unwrap();
        assert_eq!(d.payload, b"before");
        c.ack(&d).await.unwrap();
        sh(&std::env::var("KERNWAY_KAFKA_RESTART").unwrap());
        // Publishing while the broker comes back: retried until it takes it.
        let until = Instant::now() + Duration::from_secs(90);
        loop {
            match p.publish(&topic, OutMessage::new("after").key("k")).await {
                Ok(_) => break,
                Err(e) => {
                    assert!(Instant::now() < until, "no publish after the restart: {e}");
                    rt_core::sleep(Duration::from_millis(500)).await;
                }
            }
        }
        let d = c.next_timeout(Duration::from_secs(90)).await.unwrap();
        assert_eq!(d.payload, b"after", "the consumer healed; `before` stayed acknowledged");
        c.ack(&d).await.unwrap();
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

#[test]
fn a_crashed_member_hands_its_partitions_over() {
    if !live() {
        return;
    }
    block(async {
        let url = format!("{}?session_timeout=3000", std::env::var("KERNWAY_KAFKA_URL").unwrap());
        let b = KafkaBroker::connect_url(&url).await.unwrap();
        let topic = unique("crash");
        b.ensure_topic(&topic).await.unwrap();
        let mut m1 = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        let mut m2 = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        let p = b.producer().await.unwrap();
        for i in 0..30 {
            p.publish(&topic, OutMessage::new(format!("{i}")).key(format!("k{i}"))).await.unwrap();
        }
        // m2 takes some, never acknowledges, and dies without leaving.
        let mut taken = 0;
        while let Ok(_d) = m2.next_timeout(Duration::from_millis(500)).await {
            taken += 1;
        }
        assert!(taken > 0, "m2 had partitions");
        drop(m2);
        // m1 ends up with every message (its own and m2's, after the session ends).
        let mut seen = std::collections::HashSet::new();
        let until = Instant::now() + Duration::from_secs(60);
        while seen.len() < 30 {
            assert!(Instant::now() < until, "{} of 30", seen.len());
            if let Ok(d) = m1.next_timeout(Duration::from_millis(500)).await {
                seen.insert(String::from_utf8(d.payload.clone()).unwrap());
                m1.ack(&d).await.unwrap();
            }
        }
        m1.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}
