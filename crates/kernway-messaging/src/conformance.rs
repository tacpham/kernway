//! The conformance suite: the behaviour every broker backend must show
//! against a live broker. A backend crate provides a [`Harness`] and calls
//! [`run_all`] from an integration test; a backend that fails a scenario does
//! not conform. Each scenario uses fresh topics, so the suite can run
//! repeatedly against one broker. Scenarios a backend cannot support (see
//! [`Capabilities`]) are reported as `n/a`, never silently passed.

#![allow(missing_docs)]

use crate::{Broker, Capabilities, Consumer, Delivery, MsgError, OutMessage, Producer, Subscription};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

/// How the suite reaches one backend.
pub trait Harness: Clone + 'static {
    /// The broker under test.
    type Broker: Broker + Clone + 'static;
    /// Connect (on the current executor).
    fn broker(&self) -> impl Future<Output = Self::Broker>;
}

fn block<F: Future<Output = ()>>(f: F) {
    rt_core::Executor::new().expect("executor").block_on(f).expect("executor run");
}

fn unique(name: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64);
    format!("kw-conf-{name}-{}-{:x}-{}", std::process::id(), t & 0xff_ffff, N.fetch_add(1, Ordering::Relaxed))
}

const WAIT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(1500);

async fn recv<C: Consumer>(c: &mut C, what: &str) -> Delivery {
    match c.next_timeout(WAIT).await {
        Ok(d) => d,
        Err(e) => panic!("{what}: {e}"),
    }
}

async fn nothing_more<C: Consumer>(c: &mut C, what: &str) {
    match c.next_timeout(QUIET).await {
        Err(MsgError::Timeout) => {}
        Ok(d) => panic!("{what}: unexpected delivery {:?}", String::from_utf8_lossy(&d.payload)),
        Err(e) => panic!("{what}: {e}"),
    }
}

fn text(d: &Delivery) -> String {
    String::from_utf8_lossy(&d.payload).into_owned()
}

/// Run every scenario; panics with the scenario name on the first failure.
pub fn run_all<H: Harness>(h: H) {
    for (name, scenario) in [
        ("publish_confirm_and_consume", publish_confirm_and_consume::<H> as fn(H)),
        ("groups_fan_out_and_share_work", groups_fan_out_and_share_work::<H>),
        ("nack_and_close_redeliver", nack_and_close_redeliver::<H>),
        ("acks_are_durable", acks_are_durable::<H>),
        ("order_per_key", order_per_key::<H>),
        ("key_order_across_members", key_order_across_members::<H>),
        ("start_earliest_or_latest", start_earliest_or_latest::<H>),
        ("ack_timeout_redelivers", ack_timeout_redelivers::<H>),
        ("rejects_go_to_the_dead_letter_topic", rejects_go_to_the_dead_letter_topic::<H>),
        ("data_round_trips_byte_exact", data_round_trips_byte_exact::<H>),
        ("volume_without_loss_or_duplicates", volume_without_loss_or_duplicates::<H>),
        ("data_survives_redelivery_and_dead_lettering", data_survives_redelivery_and_dead_lettering::<H>),
    ] {
        eprintln!("conformance: {name}");
        scenario(h.clone());
    }
}

fn caps_of<B: Broker>(b: &B) -> Capabilities {
    b.capabilities()
}

/// Every confirmed message arrives once, payload and headers intact —
/// binary, empty, large (900 KiB: under every broker's default limit —
/// NATS caps a message at 1 MiB with its headers).
fn publish_confirm_and_consume<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        let topic = unique("basic");
        b.ensure_topic(&topic).await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g1").from_earliest()).await.unwrap();
        let p = b.producer().await.unwrap();
        let big: Vec<u8> = (0..900 * 1024).map(|i| (i % 251) as u8).collect();
        let msgs = vec![
            OutMessage::new("hello").header("x-trace", "t-1").header("x-n", "1"),
            OutMessage::new(Vec::<u8>::new()),
            OutMessage::new(vec![0u8, 255, 0, 10, 13]).key("k"),
            OutMessage::new(big.clone()),
            OutMessage::new("Việt Nam ✓ 😀"),
        ];
        for m in &msgs {
            p.publish(&topic, m.clone()).await.unwrap_or_else(|e| panic!("publish: {e}"));
        }
        // Without keys there is no order to keep (partitioned backends
        // spread them): match each delivery to the message it is.
        let mut left: Vec<&OutMessage> = msgs.iter().collect();
        for i in 0..msgs.len() {
            let d = recv(&mut c, &format!("message {i}")).await;
            let at = left.iter().position(|m| m.payload == d.payload).unwrap_or_else(|| panic!("unexpected or repeated payload of {} bytes", d.payload.len()));
            let want = left.remove(at);
            for (k, v) in &want.headers {
                assert_eq!(d.header(k), Some(v.as_str()), "header {k}");
            }
            assert_eq!(d.key, want.key, "the key travels with the message");
            assert!(!d.redelivered, "first delivery");
            c.ack(&d).await.unwrap();
        }
        nothing_more(&mut c, "after all were acknowledged").await;
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

/// Every group gets every message; members of one group split them.
fn groups_fan_out_and_share_work<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        let topic = unique("groups");
        b.ensure_topic(&topic).await.unwrap();
        let mut a1 = b.subscribe(Subscription::new(&topic, "a").from_earliest().max_in_flight(1)).await.unwrap();
        let mut a2 = b.subscribe(Subscription::new(&topic, "a").from_earliest().max_in_flight(1)).await.unwrap();
        let mut other = b.subscribe(Subscription::new(&topic, "b").from_earliest()).await.unwrap();
        let p = b.producer().await.unwrap();
        let n = 40;
        for i in 0..n {
            p.publish(&topic, OutMessage::new(format!("m{i}"))).await.unwrap();
        }
        // Group b alone: everything.
        let mut seen_b = HashSet::new();
        for _ in 0..n {
            let d = recv(&mut other, "group b").await;
            seen_b.insert(text(&d));
            other.ack(&d).await.unwrap();
        }
        assert_eq!(seen_b.len(), n);
        // Group a: the two members alternate; together everything, once.
        let mut seen_a = Vec::new();
        let mut per = [0usize; 2];
        while seen_a.len() < n {
            for (i, c) in [&mut a1, &mut a2].into_iter().enumerate() {
                if let Ok(d) = c.next_timeout(Duration::from_millis(300)).await {
                    seen_a.push(text(&d));
                    per[i] += 1;
                    c.ack(&d).await.unwrap();
                }
            }
            assert!(seen_a.len() <= n, "a message went to both members");
        }
        let unique_a: HashSet<_> = seen_a.iter().cloned().collect();
        assert_eq!(unique_a.len(), n, "every message reached group a once");
        assert!(per[0] > 0 && per[1] > 0, "both members got work: {per:?}");
        nothing_more(&mut a1, "group a, member 1").await;
        for c in [a1, a2, other] {
            c.close().await.unwrap();
        }
        b.delete_topic(&topic).await.unwrap();
    });
}

/// A nacked delivery comes again (flagged); unacknowledged deliveries of a
/// consumer that closes go to another member.
fn nack_and_close_redeliver<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        let topic = unique("redeliver");
        b.ensure_topic(&topic).await.unwrap();
        let mut c1 = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        let p = b.producer().await.unwrap();
        p.publish(&topic, OutMessage::new("once more")).await.unwrap();
        let d = recv(&mut c1, "first").await;
        c1.nack(&d, true).await.unwrap();
        let again = recv(&mut c1, "after nack").await;
        assert_eq!(text(&again), "once more");
        assert!(again.redelivered, "flagged as redelivered");
        c1.ack(&again).await.unwrap();
        // Unacknowledged when its consumer leaves.
        for i in 0..3 {
            p.publish(&topic, OutMessage::new(format!("held {i}"))).await.unwrap();
        }
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(text(&recv(&mut c1, "held").await));
        }
        c1.close().await.unwrap();
        let mut c2 = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        let mut got = Vec::new();
        for _ in 0..3 {
            let d = recv(&mut c2, "taken over").await;
            assert!(d.redelivered, "{} was delivered before", text(&d));
            got.push(text(&d));
            c2.ack(&d).await.unwrap();
        }
        got.sort();
        held.sort();
        assert_eq!(got, held);
        nothing_more(&mut c2, "after the takeover").await;
        c2.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

/// Acknowledged messages are not delivered again to the group, also after
/// every member left and a new one joined.
fn acks_are_durable<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        let topic = unique("durable");
        b.ensure_topic(&topic).await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        let p = b.producer().await.unwrap();
        for i in 0..10 {
            p.publish(&topic, OutMessage::new(format!("d{i}"))).await.unwrap();
        }
        let mut acked = HashSet::new();
        for _ in 0..6 {
            let d = recv(&mut c, "first six").await;
            acked.insert(text(&d));
            c.ack(&d).await.unwrap();
        }
        c.close().await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        let mut rest = HashSet::new();
        for _ in 0..4 {
            let d = recv(&mut c, "the rest").await;
            assert!(!acked.contains(&text(&d)), "{} was acknowledged, yet came again", text(&d));
            rest.insert(text(&d));
            c.ack(&d).await.unwrap();
        }
        let all: HashSet<String> = (0..10).map(|i| format!("d{i}")).collect();
        assert_eq!(rest, &all - &acked);
        nothing_more(&mut c, "after the rest").await;
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

/// Messages with one key arrive in publish order.
fn order_per_key<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        let topic = unique("order");
        b.ensure_topic(&topic).await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        let p = b.producer().await.unwrap();
        let n = 200;
        for i in 0..n {
            p.publish(&topic, OutMessage::new(format!("{i}")).key(format!("k{}", i % 4))).await.unwrap();
        }
        let mut last: HashMap<Vec<u8>, i64> = HashMap::new();
        for _ in 0..n {
            let d = recv(&mut c, "ordered").await;
            let i: i64 = text(&d).parse().unwrap();
            let key = d.key.clone().unwrap_or_default();
            let prev = last.insert(key.clone(), i).unwrap_or(-1);
            assert!(i > prev, "key {:?}: {i} after {prev}", String::from_utf8_lossy(&key));
            assert_eq!(key, format!("k{}", i % 4).into_bytes(), "the key travels with the message");
            c.ack(&d).await.unwrap();
        }
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

/// With two members holding deliveries at the same time, neither gets a
/// key's message while an earlier one of that key is unacknowledged.
fn key_order_across_members<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        if !caps_of(&b).key_order_across_members {
            eprintln!("n/a: key order across members");
            return;
        }
        let topic = unique("keyorder");
        b.ensure_topic(&topic).await.unwrap();
        let mut m1 = b.subscribe(Subscription::new(&topic, "g").from_earliest().max_in_flight(4)).await.unwrap();
        let mut m2 = b.subscribe(Subscription::new(&topic, "g").from_earliest().max_in_flight(4)).await.unwrap();
        let p = b.producer().await.unwrap();
        let n = 60;
        for i in 0..n {
            p.publish(&topic, OutMessage::new(format!("{i}")).key(format!("k{}", i % 3))).await.unwrap();
        }
        let mut done = 0;
        let mut last: HashMap<Vec<u8>, i64> = HashMap::new();
        let until = std::time::Instant::now() + Duration::from_secs(60);
        while done < n {
            assert!(std::time::Instant::now() < until, "only {done} of {n} handled");
            // Both members take what they can before anything is acknowledged.
            let mut round: Vec<(usize, Delivery)> = Vec::new();
            for (who, c) in [&mut m1, &mut m2].into_iter().enumerate() {
                for _ in 0..4 {
                    match c.next_timeout(Duration::from_millis(150)).await {
                        Ok(d) => round.push((who, d)),
                        Err(_) => break,
                    }
                }
            }
            let mut keys = HashSet::new();
            for (_, d) in &round {
                let key = d.key.clone().unwrap_or_default();
                assert!(keys.insert(key.clone()), "two deliveries of key {:?} in flight at once", String::from_utf8_lossy(&key));
            }
            for (who, d) in round {
                let i: i64 = text(&d).parse().unwrap();
                let key = d.key.clone().unwrap_or_default();
                let prev = last.insert(key, i).unwrap_or(-1);
                assert!(i > prev, "key order: {i} after {prev}");
                let c = if who == 0 { &mut m1 } else { &mut m2 };
                c.ack(&d).await.unwrap();
                done += 1;
            }
        }
        for c in [m1, m2] {
            c.close().await.unwrap();
        }
        b.delete_topic(&topic).await.unwrap();
    });
}

/// A new group starts after the newest message (every backend), or at the
/// oldest one (backends that replay).
fn start_earliest_or_latest<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        let replay = caps_of(&b).replay;
        let topic = unique("start");
        b.ensure_topic(&topic).await.unwrap();
        let p = b.producer().await.unwrap();
        // A group that exists keeps what arrives while no member is connected.
        let keeper = b.subscribe(Subscription::new(&topic, "keeper")).await.unwrap();
        keeper.close().await.unwrap();
        p.publish(&topic, OutMessage::new("old")).await.unwrap();
        let mut early = if replay { Some(b.subscribe(Subscription::new(&topic, "early").from_earliest()).await.unwrap()) } else { None };
        let mut late = b.subscribe(Subscription::new(&topic, "late")).await.unwrap();
        // Let a lazily-attached consumer settle before the new message.
        let _ = late.next_timeout(Duration::from_millis(500)).await.map(|d| panic!("late saw {:?}", text(&d)));
        p.publish(&topic, OutMessage::new("new")).await.unwrap();
        if let Some(early) = early.as_mut() {
            let mut got = [text(&recv(early, "earliest").await), text(&recv(early, "earliest").await)];
            got.sort();
            assert_eq!(got, ["new", "old"]);
        } else {
            eprintln!("n/a: no replay for a new group (the latest half ran)");
        }
        let d = recv(&mut late, "latest").await;
        assert_eq!(text(&d), "new", "latest skips what was published before");
        late.ack(&d).await.unwrap();
        let mut keeper = b.subscribe(Subscription::new(&topic, "keeper")).await.unwrap();
        let mut kept = Vec::new();
        for _ in 0..2 {
            let d = recv(&mut keeper, "kept while away").await;
            kept.push(text(&d));
            keeper.ack(&d).await.unwrap();
        }
        kept.sort();
        assert_eq!(kept, ["new", "old"]);
        for c in [Some(late), Some(keeper), early].into_iter().flatten() {
            c.close().await.unwrap();
        }
        b.delete_topic(&topic).await.unwrap();
    });
}

/// A delivery left unacknowledged past `ack_wait` is delivered again.
fn ack_timeout_redelivers<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        if !caps_of(&b).ack_timeout {
            eprintln!("n/a: no ack timeout while connected");
            return;
        }
        let topic = unique("ackwait");
        b.ensure_topic(&topic).await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g").from_earliest().ack_wait(Duration::from_secs(2))).await.unwrap();
        let p = b.producer().await.unwrap();
        p.publish(&topic, OutMessage::new("slow")).await.unwrap();
        let first = recv(&mut c, "first").await;
        let again = recv(&mut c, "after the ack wait").await;
        assert_eq!(text(&again), "slow");
        assert!(again.redelivered);
        let _ = c.ack(&first).await;
        c.ack(&again).await.unwrap();
        nothing_more(&mut c, "after the ack").await;
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

/// A rejected delivery lands on the dead-letter topic, and not again on
/// its own.
fn rejects_go_to_the_dead_letter_topic<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        if !caps_of(&b).dead_letter {
            eprintln!("n/a: no dead-letter topics");
            return;
        }
        let topic = unique("dlq");
        let dead = format!("{topic}-dead");
        b.ensure_topic(&topic).await.unwrap();
        b.ensure_topic(&dead).await.unwrap();
        let mut dl = b.subscribe(Subscription::new(&dead, "inspect").from_earliest()).await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g").from_earliest().dead_letter(&dead)).await.unwrap();
        let p = b.producer().await.unwrap();
        // One key: "poison" comes first on every backend.
        p.publish(&topic, OutMessage::new("poison").header("x-id", "7").key("order")).await.unwrap();
        p.publish(&topic, OutMessage::new("fine").key("order")).await.unwrap();
        let d = recv(&mut c, "poison").await;
        assert_eq!(text(&d), "poison");
        c.nack(&d, false).await.unwrap();
        let ok = recv(&mut c, "fine").await;
        assert_eq!(text(&ok), "fine");
        c.ack(&ok).await.unwrap();
        nothing_more(&mut c, "the rejected one is not redelivered").await;
        let x = recv(&mut dl, "dead letter").await;
        assert_eq!(text(&x), "poison");
        assert_eq!(x.header("x-id"), Some("7"), "headers survive dead-lettering");
        dl.ack(&x).await.unwrap();
        for c in [c, dl] {
            c.close().await.unwrap();
        }
        b.delete_topic(&topic).await.unwrap();
        b.delete_topic(&dead).await.unwrap();
    });
}

/// The data a message carries arrives exactly as sent: every byte value,
/// empty and large payloads, every script of UTF-8; headers with Unicode,
/// empty, long and punctuated values, many headers, repeated names,
/// case-insensitive lookup; binary keys, and an empty key apart from none.
fn data_round_trips_byte_exact<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        let topic = unique("data");
        b.ensure_topic(&topic).await.unwrap();
        let mut c = b.subscribe(Subscription::new(&topic, "g").from_earliest()).await.unwrap();
        let p = b.producer().await.unwrap();
        let every_byte: Vec<u8> = (0..=255u8).cycle().take(256 * 4).collect();
        let long_value = "v".repeat(4096);
        let mut many = OutMessage::new("forty headers");
        for i in 0..40 {
            many = many.header(format!("h-{i:02}"), format!("value {i}"));
        }
        let msgs = vec![
            OutMessage::new(every_byte.clone()).key("bytes"),
            OutMessage::new(Vec::<u8>::new()).key("bytes"),
            OutMessage::new(vec![0u8]).key("bytes"),
            OutMessage::new("Tiếng Việt có dấu: ắ ằ ẳ ẵ ặ · 中文 · 日本語 · 한국어 · العربية · עברית · 😀👍🏽 · \u{0}"),
            OutMessage::new(r#"{"ticket":42,"items":[{"name":"Gel \"French\"","cents":3500}],"note":null}"#).header("content-type", "application/json"),
            OutMessage::new((0..900 * 1024).map(|i| (i * 7 % 256) as u8).collect::<Vec<u8>>()),
            OutMessage::new("headers")
                .header("x-unicode", "Chào chị chủ tiệm 💅")
                .header("x-empty", "")
                .header("x-long", long_value.as_str())
                .header("x-punct", "a: b = c; d, e  f")
                .header("X-Mixed-Case", "kept")
                .header("x-repeat", "one")
                .header("x-repeat", "two"),
            many,
            OutMessage::new("binary key").key(vec![0u8, 1, 0xfe, 0xff, b'\n']),
            OutMessage::new("empty key").key(Vec::<u8>::new()),
            OutMessage::new("no key"),
        ];
        for m in &msgs {
            p.publish(&topic, m.clone()).await.unwrap_or_else(|e| panic!("publish: {e}"));
        }
        let mut left: Vec<&OutMessage> = msgs.iter().collect();
        for i in 0..msgs.len() {
            let d = recv(&mut c, &format!("data {i}")).await;
            let at = left
                .iter()
                .position(|m| m.payload == d.payload && m.key == d.key)
                .unwrap_or_else(|| panic!("a delivery matching no sent message: {} bytes, key {:?}", d.payload.len(), d.key));
            let want = left.remove(at);
            for (k, v) in &want.headers {
                let got: Vec<&str> = d.headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str()).collect();
                assert!(got.contains(&v.as_str()), "header {k}: want {v:?}, got {got:?}");
            }
            if want.payload == b"headers" {
                assert_eq!(d.header("x-mixed-case"), Some("kept"), "names are looked up case-insensitively");
                let repeats: Vec<&str> = d.headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("x-repeat")).map(|(_, v)| v.as_str()).collect();
                assert_eq!(repeats, ["one", "two"], "repeated names keep every value, in order");
                assert_eq!(d.header("x-empty"), Some(""), "an empty value is not dropped");
            }
            c.ack(&d).await.unwrap();
        }
        // Keyed messages of one key arrive in order (the three "bytes" ones).
        nothing_more(&mut c, "after all data").await;
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

/// Several producers at once, many messages, two members: every confirmed
/// message arrives exactly once (nothing fails here, so nothing is
/// redelivered), each member sees a key's messages in publish order.
fn volume_without_loss_or_duplicates<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        let topic = unique("volume");
        b.ensure_topic(&topic).await.unwrap();
        let mut m1 = b.subscribe(Subscription::new(&topic, "g").from_earliest().max_in_flight(32)).await.unwrap();
        let mut m2 = b.subscribe(Subscription::new(&topic, "g").from_earliest().max_in_flight(32)).await.unwrap();
        const PRODUCERS: usize = 3;
        const EACH: usize = 400;
        let done = std::rc::Rc::new(std::cell::Cell::new(0usize));
        for pi in 0..PRODUCERS {
            let (b, topic, done) = (b.clone(), topic.clone(), done.clone());
            rt_core::spawn(async move {
                let p = b.producer().await.unwrap();
                for i in 0..EACH {
                    // Key per producer and lane: order is per producer within a key.
                    let key = format!("p{pi}-k{}", i % 10);
                    let body = format!("{pi}:{i}:{}", "x".repeat(i % 300));
                    p.publish(&topic, OutMessage::new(body).key(key).header("seq", i.to_string())).await.unwrap();
                }
                done.set(done.get() + 1);
            });
        }
        let total = PRODUCERS * EACH;
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        let mut last: [HashMap<String, usize>; 2] = [HashMap::new(), HashMap::new()];
        let until = std::time::Instant::now() + Duration::from_secs(120);
        while seen.len() < total {
            assert!(std::time::Instant::now() < until, "{} of {total} arrived (producers done: {})", seen.len(), done.get());
            for (who, c) in [&mut m1, &mut m2].into_iter().enumerate() {
                let Ok(d) = c.next_timeout(Duration::from_millis(100)).await else { continue };
                let t = text(&d);
                let mut parts = t.splitn(3, ':');
                let (pi, i): (usize, usize) = (parts.next().unwrap().parse().unwrap(), parts.next().unwrap().parse().unwrap());
                assert_eq!(parts.next().unwrap().len(), i % 300, "payload intact");
                assert_eq!(d.header("seq"), Some(i.to_string().as_str()));
                assert!(seen.insert((pi, i)), "duplicate {pi}:{i}");
                let key = String::from_utf8(d.key.clone().unwrap()).unwrap();
                if let Some(prev) = last[who].insert(key.clone(), i) {
                    assert!(i > prev, "key {key}: {i} after {prev} at one member");
                }
                c.ack(&d).await.unwrap();
            }
        }
        assert_eq!(done.get(), PRODUCERS, "every publish was confirmed");
        nothing_more(&mut m1, "after the volume").await;
        nothing_more(&mut m2, "after the volume").await;
        for c in [m1, m2] {
            c.close().await.unwrap();
        }
        b.delete_topic(&topic).await.unwrap();
    });
}

/// A message's payload, headers and key are unchanged when it comes again
/// after a nack and when it is dead-lettered.
fn data_survives_redelivery_and_dead_lettering<H: Harness>(h: H) {
    block(async move {
        let b = h.broker().await;
        let topic = unique("intact");
        let dead = format!("{topic}-dead");
        b.ensure_topic(&topic).await.unwrap();
        let dl_capable = caps_of(&b).dead_letter;
        let mut sub = Subscription::new(&topic, "g").from_earliest();
        let mut dl = None;
        if dl_capable {
            b.ensure_topic(&dead).await.unwrap();
            dl = Some(b.subscribe(Subscription::new(&dead, "inspect").from_earliest()).await.unwrap());
            sub = sub.dead_letter(&dead);
        }
        let mut c = b.subscribe(sub).await.unwrap();
        let payload: Vec<u8> = (0..=255u8).rev().cycle().take(5000).collect();
        let sent = OutMessage::new(payload.clone()).key(vec![0u8, 0xff, b'k']).header("x-unicode", "Đơn hàng #42 ✓").header("x-empty", "");
        b.producer().await.unwrap().publish(&topic, sent.clone()).await.unwrap();
        let same = |d: &Delivery, what: &str| {
            assert_eq!(d.payload, sent.payload, "{what}: payload");
            assert_eq!(d.key, sent.key, "{what}: key");
            assert_eq!(d.header("x-unicode"), Some("Đơn hàng #42 ✓"), "{what}: header");
            assert_eq!(d.header("x-empty"), Some(""), "{what}: empty header");
        };
        let first = recv(&mut c, "first").await;
        same(&first, "first delivery");
        c.nack(&first, true).await.unwrap();
        let again = recv(&mut c, "again").await;
        same(&again, "redelivery");
        assert!(again.redelivered);
        if let Some(mut dl) = dl {
            c.nack(&again, false).await.unwrap();
            let x = recv(&mut dl, "dead letter").await;
            same(&x, "dead letter");
            dl.ack(&x).await.unwrap();
            dl.close().await.unwrap();
            b.delete_topic(&dead).await.unwrap();
        } else {
            c.ack(&again).await.unwrap();
            eprintln!("n/a: no dead-letter topics (the redelivery half ran)");
        }
        c.close().await.unwrap();
        b.delete_topic(&topic).await.unwrap();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryBroker;

    #[derive(Clone)]
    struct Mem;

    impl Harness for Mem {
        type Broker = MemoryBroker;
        async fn broker(&self) -> MemoryBroker {
            MemoryBroker::new()
        }
    }

    #[test]
    fn the_memory_broker_conforms() {
        run_all(Mem);
    }
}
