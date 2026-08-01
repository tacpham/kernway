//! Smoke test for the new pub/sub surface: one connection SUBSCRIBEs, another PUBLISHes,
//! and the subscriber receives the message. Run against a local Redis:
//!   cargo run -p kernway-redis --example pubsub_smoke
//! (expects Redis on 127.0.0.1:6380 — the kw-redis dev container).

use kernway_redis::conn::Connection;

fn main() {
    let ex = rt_core::Executor::new().expect("runtime");
    let ok = ex.block_on(async {
        let addr = "127.0.0.1:6380".parse().expect("addr");
        // Two independent connections: a subscriber must be dedicated (RESP2).
        let mut sub = Connection::connect(addr).await.expect("connect sub");
        let mut pubc = Connection::connect(addr).await.expect("connect pub");

        sub.subscribe(&["kw:crawl"]).await.expect("subscribe");
        // Subscribe has returned its confirmation, so the subscriber is live — publish now.
        let n = pubc.publish("kw:crawl", b"{\"requestId\":\"r1\",\"url\":\"https://audible.com/pd/x\"}").await.expect("publish");
        println!("PUBLISH delivered to {n} subscriber(s)");

        let (channel, payload) = sub.next_message().await.expect("next_message");
        println!("RECEIVED channel={channel} payload={}", String::from_utf8_lossy(&payload));
        n == 1 && channel == "kw:crawl" && payload.starts_with(b"{\"requestId\"")
    }).expect("runtime error");
    println!("PUBSUB_SMOKE {}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}
