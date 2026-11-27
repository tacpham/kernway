//! Redis Cluster against a real 3-master / 3-replica cluster
//! (tests/support/redis-cluster-setup.sh, `KERNWAY_REDIS_CLUSTER_URL`):
//! slot routing, hash tags, cross-slot errors, pipelines split by node,
//! transactions, Lua, scans, sharded pub/sub — and a live slot migration
//! (ASK during it, MOVED after it) and a failover, both followed.
//!
//! One test function: the migration and failover change the cluster, and
//! must not race the other scenarios.

use kernway_redis::{key_slot, ClusterClient, Cmd, RedisError, Value};
use std::time::{Duration, Instant};

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

async fn node_id(c: &ClusterClient, (h, p): &(String, u16)) -> String {
    c.query_node(h, *p, Cmd::new("CLUSTER").arg("MYID")).await.unwrap()
}

/// Move `slot` (holding `key`) from `src` to `dst` the way redis-cli does —
/// every key of the slot, since other test keys may share it — pausing after
/// the keys moved (the ASK window) to run `during`.
async fn migrate_slot<F>(c: &ClusterClient, slot: u16, key: &str, src: &(String, u16), dst: &(String, u16), during: F)
where
    F: std::future::Future<Output = ()>,
{
    let (src_id, dst_id) = (node_id(c, src).await, node_id(c, dst).await);
    let q = |n: &(String, u16), cmd: Cmd| {
        let n = n.clone();
        async move { c.query_node::<Value>(&n.0, n.1, cmd).await.unwrap() }
    };
    q(dst, Cmd::new("CLUSTER").arg("SETSLOT").arg(slot).arg("IMPORTING").arg(&src_id)).await;
    q(src, Cmd::new("CLUSTER").arg("SETSLOT").arg(slot).arg("MIGRATING").arg(&dst_id)).await;
    let held = q(src, Cmd::new("CLUSTER").arg("GETKEYSINSLOT").arg(slot).arg(10_000)).await;
    let keys: Vec<String> = held.as_items().unwrap_or_default().iter().filter_map(Value::as_text).collect();
    assert!(keys.iter().any(|k| k == key), "{key} is in slot {slot}: {keys:?}");
    q(src, Cmd::new("MIGRATE").arg(dst.0.as_str()).arg(dst.1).arg("").arg(0).arg(5000).arg("KEYS").args(&keys)).await;
    during.await;
    for shard in c.shards() {
        q(&shard.master, Cmd::new("CLUSTER").arg("SETSLOT").arg(slot).arg("NODE").arg(&dst_id)).await;
    }
}

#[test]
fn cluster_routing_redirections_and_failover() {
    let Ok(url) = std::env::var("KERNWAY_REDIS_CLUSTER_URL") else {
        return eprintln!("skip: KERNWAY_REDIS_CLUSTER_URL not set");
    };
    run(async move {
        let pid = std::process::id();
        // A previous run's failover may still be settling: wait for a stable
        // cluster (every slot served, three masters with one replica each).
        let started = Instant::now();
        loop {
            let probe = ClusterClient::from_url(&url).await.unwrap();
            let info: String = probe.query(Cmd::new("CLUSTER").arg("INFO")).await.unwrap();
            let shards = probe.shards();
            if info.contains("cluster_state:ok") && shards.len() == 3 && shards.iter().all(|s| s.replicas.len() == 1) {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "cluster not stable: {info} {shards:?}");
            rt_core::sleep(Duration::from_millis(200)).await;
        }
        // A dead seed listed first is skipped.
        let c = ClusterClient::from_url(&url.replace("redis+cluster://", "redis+cluster://127.0.0.1:1,")).await.unwrap();
        let shards = c.shards();
        assert_eq!(shards.len(), 3, "{shards:?}");
        assert!(shards.iter().all(|s| s.replicas.len() == 1), "{shards:?}");

        // Keys spread over every master, each stored on the node owning its slot.
        let keys: Vec<String> = (0..300).map(|i| format!("kwcl:{pid}:{i}")).collect();
        for k in &keys {
            c.query::<()>(Cmd::new("SET").arg(k).arg(k)).await.unwrap();
        }
        let mut owners = std::collections::HashSet::new();
        for k in keys.iter().take(30) {
            let (h, p) = c.node_for(k.as_bytes()).unwrap();
            owners.insert(p);
            assert_eq!(c.query_node::<String>(&h, p, Cmd::new("GET").arg(k)).await.unwrap(), *k, "stored on its owner");
            let other = shards.iter().find(|s| s.master.1 != p).unwrap();
            let e = c.query_node::<Value>(&other.master.0, other.master.1, Cmd::new("GET").arg(k)).await.unwrap_err();
            assert!(matches!(&e, RedisError::Server(m) if m.starts_with("MOVED")), "{e}");
        }
        assert_eq!(owners.len(), 3, "keys on all three masters");
        let mut found = c.scan_all(&format!("kwcl:{pid}:*")).await.unwrap();
        found.sort();
        let mut want = keys.clone();
        want.sort();
        assert_eq!(found, want, "SCAN across every master");

        // Hash tags: multi-key commands and transactions on one slot.
        let (t1, t2) = (format!("{{kwtag{pid}}}:a"), format!("{{kwtag{pid}}}:b"));
        assert_eq!(key_slot(t1.as_bytes()), key_slot(t2.as_bytes()));
        c.query::<()>(Cmd::new("MSET").arg(&t1).arg(1).arg(&t2).arg(2)).await.unwrap();
        let both: Vec<i64> = c.query(Cmd::new("MGET").arg(&t1).arg(&t2)).await.unwrap();
        assert_eq!(both, [1, 2]);
        let r = c.exec(&[t1.as_str()], &[Cmd::new("INCR").arg(&t1), Cmd::new("INCR").arg(&t2)]).await.unwrap().unwrap();
        assert_eq!(r.iter().map(|v| v.as_ref().unwrap().as_int().unwrap()).collect::<Vec<_>>(), [2, 3]);
        // Different slots: the server refuses a multi-key command, the client
        // refuses a transaction before sending it.
        let e = c.query::<Value>(Cmd::new("MGET").arg(&keys[0]).arg(&keys[1])).await.unwrap_err();
        assert!(matches!(&e, RedisError::Server(m) if m.starts_with("CROSSSLOT")), "{e}");
        let e = c.exec(&[], &[Cmd::new("INCR").arg(&keys[0]), Cmd::new("INCR").arg(&keys[1])]).await.unwrap_err();
        assert!(matches!(&e, RedisError::Config(m) if m.contains("hash tag")), "{e}");

        // A pipeline over every node, replies in the original order.
        let cmds: Vec<Cmd> = keys.iter().take(100).map(|k| Cmd::new("GET").arg(k)).collect();
        let replies = c.pipeline(&cmds).await.unwrap();
        for (k, r) in keys.iter().zip(&replies) {
            assert_eq!(r.as_ref().unwrap().as_text().unwrap(), *k);
        }
        // Lua routed by its key.
        let n: i64 = c.query(Cmd::new("EVAL").arg("return redis.call('STRLEN', KEYS[1])").arg(1).arg(&keys[7])).await.unwrap();
        assert_eq!(n as usize, keys[7].len());

        // Sharded pub/sub on the channel's node.
        let ch = format!("kwsch:{pid}");
        let mut sub = c.ssubscribe(&[&ch]).await.unwrap();
        let receivers: i64 = c.query(Cmd::new("SPUBLISH").arg(&ch).arg("xin chào")).await.unwrap();
        assert_eq!(receivers, 1);
        let m = sub.next_pubsub().await.unwrap();
        assert_eq!((m.channel.as_str(), m.payload.as_slice()), (ch.as_str(), "xin chào".as_bytes()));

        // Reads from replicas: GETs land on replicas (their command stats
        // grow), writes still on masters.
        let rc = ClusterClient::from_url(&url).await.unwrap().read_from_replicas(true);
        let k0 = &keys[0];
        let replica_of_k0 = rc.shards().into_iter().find(|s| Some(&s.master) == rc.node_for(k0.as_bytes()).as_ref()).unwrap().replicas[0].clone();
        let gets = |info: String| -> u64 {
            info.lines().find_map(|l| l.strip_prefix("cmdstat_get:calls=")).and_then(|v| v.split(',').next()).and_then(|v| v.parse().ok()).unwrap_or(0)
        };
        let before: u64 = gets(rc.query_node(&replica_of_k0.0, replica_of_k0.1, Cmd::new("INFO").arg("commandstats")).await.unwrap());
        // The replica has it once replication caught up.
        let started = Instant::now();
        while rc.query::<Option<String>>(Cmd::new("GET").arg(k0)).await.unwrap().as_deref() != Some(k0.as_str()) {
            assert!(started.elapsed() < Duration::from_secs(10), "replica never caught up");
            rt_core::sleep(Duration::from_millis(50)).await;
        }
        let after: u64 = gets(rc.query_node(&replica_of_k0.0, replica_of_k0.1, Cmd::new("INFO").arg("commandstats")).await.unwrap());
        assert!(after > before, "the GET ran on the replica ({before} → {after})");
        rc.query::<()>(Cmd::new("SET").arg(k0).arg(k0.as_str())).await.unwrap();

        // A live slot migration: ASK while the key has moved but the slot has
        // not (the map stays), MOVED once it has (the map follows).
        let k = &keys[3];
        let slot = key_slot(k.as_bytes());
        let src = c.node_for(k.as_bytes()).unwrap();
        // Another key in the same slot: a slot moves with all its keys.
        let neighbour = (0..).map(|i| format!("kwcl:{pid}:n{i}")).find(|n| key_slot(n.as_bytes()) == slot).unwrap();
        c.query::<()>(Cmd::new("SET").arg(&neighbour).arg("next door")).await.unwrap();
        let dst = shards.iter().find(|s| s.master != src).unwrap().master.clone();
        let fresh = ClusterClient::from_url(&url).await.unwrap(); // a client with the old map
        migrate_slot(&c, slot, k, &src, &dst, async {
            assert_eq!(fresh.query::<String>(Cmd::new("GET").arg(k)).await.unwrap(), *k, "followed ASK");
            assert_eq!(fresh.node_for(k.as_bytes()).unwrap(), src, "ASK does not change the map");
        })
        .await;
        assert_eq!(fresh.query::<String>(Cmd::new("GET").arg(k)).await.unwrap(), *k, "followed MOVED");
        assert_eq!(fresh.node_for(k.as_bytes()).unwrap(), dst, "MOVED updated the map");
        assert_eq!(fresh.query::<String>(Cmd::new("GET").arg(&neighbour)).await.unwrap(), "next door", "moved with the slot");
        // Put the slot back where it was, and check it is.
        migrate_slot(&c, slot, k, &dst, &src, async {}).await;
        let check = ClusterClient::from_url(&url).await.unwrap();
        assert_eq!(check.node_for(k.as_bytes()).unwrap(), src, "slot {slot} back on its owner");

        // A failover: the replica of k's shard takes over; the client follows.
        let k = &keys[11];
        let owner = c.node_for(k.as_bytes()).unwrap();
        let replica = c.shards().into_iter().find(|s| s.master == owner).unwrap().replicas[0].clone();
        c.query_node::<()>(&replica.0, replica.1, Cmd::new("CLUSTER").arg("FAILOVER")).await.unwrap();
        // Done when both sides switched roles and a third node sees the
        // promoted replica owning the slot.
        let started = Instant::now();
        let role_of = |n: (String, u16)| {
            let c = &c;
            async move {
                let r: Result<Value, _> = c.query_node(&n.0, n.1, Cmd::new("ROLE")).await;
                r.ok().and_then(|r| r.as_items().and_then(|i| i.first()).and_then(Value::as_text)).unwrap_or_default()
            }
        };
        let third = c.shards().into_iter().find(|s| s.master != owner).unwrap().master;
        let mut took_over = false;
        loop {
            // Masters will not vote again for this shard within 2 × node
            // timeout of their last vote (back-to-back runs): take over.
            if !took_over && started.elapsed() > Duration::from_secs(8) {
                let _ = c.query_node::<()>(&replica.0, replica.1, Cmd::new("CLUSTER").arg("FAILOVER").arg("TAKEOVER")).await;
                took_over = true;
            }
            let seen_by_third = {
                let view = ClusterClient::connect(&[(third.0.as_str(), third.1)], kernway_redis::Options::tcp("", 0)).await;
                view.ok().and_then(|v| v.node_for(k.as_bytes()))
            };
            if role_of(replica.clone()).await == "master" && role_of(owner.clone()).await == "slave" && seen_by_third.as_ref() == Some(&replica) {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "failover did not complete");
            rt_core::sleep(Duration::from_millis(100)).await;
        }
        c.query::<()>(Cmd::new("SET").arg(k).arg("after failover")).await.unwrap();
        assert_eq!(c.node_for(k.as_bytes()).unwrap(), replica, "the promoted replica owns the slot now");
        assert_eq!(c.query::<String>(Cmd::new("GET").arg(k)).await.unwrap(), "after failover");

        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        for chunk in refs.chunks(1) {
            c.query::<()>(Cmd::new("DEL").arg(chunk[0])).await.unwrap();
        }
        c.query::<()>(Cmd::new("DEL").arg(&t1).arg(&t2)).await.unwrap();
    });
}
