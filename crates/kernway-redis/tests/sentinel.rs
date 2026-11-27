//! Redis Sentinel against a real deployment (tests/support/redis-sentinel-setup.sh:
//! master + replica + three sentinels, `KERNWAY_REDIS_SENTINEL_URL`):
//! finding the master, skipping a dead sentinel, replicas for reads, unknown
//! groups, and a real failover the pool follows.

use kernway_redis::{Cmd, Connection, Options, Pool, RedisError, Value};
use std::time::{Duration, Instant};

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

async fn port_of(c: &mut Connection) -> String {
    let info: String = c.query(Cmd::new("INFO").arg("server")).await.unwrap();
    info.lines().find_map(|l| l.strip_prefix("tcp_port:")).unwrap().trim().to_string()
}

async fn master_port(url: &str) -> String {
    let sentinel = url.trim_start_matches("redis+sentinel://").split(',').next().unwrap().to_string();
    let mut s = Connection::open(&Options::from_url(&format!("redis://{sentinel}?protocol=2")).unwrap()).await.unwrap();
    let addr: Vec<String> = s.query(Cmd::new("SENTINEL").arg("GET-MASTER-ADDR-BY-NAME").arg("kwmaster")).await.unwrap();
    addr[1].clone()
}

#[test]
fn sentinel_master_replica_and_failover() {
    let Ok(url) = std::env::var("KERNWAY_REDIS_SENTINEL_URL") else {
        return eprintln!("skip: KERNWAY_REDIS_SENTINEL_URL not set");
    };
    run(async move {
        let key = format!("kwsentinel:{}", std::process::id());

        // The master, wherever the last run's failover left it.
        let before = master_port(&url).await;
        let mut m = Connection::open(&Options::from_url(&url).unwrap()).await.unwrap();
        assert_eq!(port_of(&mut m).await, before);
        let role: Value = m.query(Cmd::new("ROLE")).await.unwrap();
        assert_eq!(role.as_items().unwrap()[0].as_text().unwrap(), "master");
        m.query::<()>(Cmd::new("SET").arg(&key).arg("v1")).await.unwrap();

        // A dead sentinel listed first is skipped.
        let with_dead = url.replace("redis+sentinel://", "redis+sentinel://127.0.0.1:1,");
        let mut m2 = Connection::open(&Options::from_url(&with_dead).unwrap()).await.unwrap();
        assert_eq!(port_of(&mut m2).await, before);

        // A replica for reads: it has the data, it refuses writes, and the
        // refusal does not break the connection.
        let mut r = Connection::open(&Options::from_url(&format!("{url}?replica=true")).unwrap()).await.unwrap();
        assert_ne!(port_of(&mut r).await, before);
        // Right after an earlier failover the replica may still be resyncing:
        // wait for it to have the value.
        let started = Instant::now();
        while r.get(&key).await.unwrap() != Some(b"v1".to_vec()) {
            assert!(started.elapsed() < Duration::from_secs(10), "the replica never caught up");
            rt_core::sleep(Duration::from_millis(50)).await;
        }
        assert!(matches!(r.query::<()>(Cmd::new("SET").arg(&key).arg("x")).await, Err(RedisError::Server(m)) if m.starts_with("READONLY")));
        assert!(!r.is_broken());

        // An unknown group, and no sentinel at all.
        let unknown = url.replace("/kwmaster", "/nosuchgroup");
        let e = Connection::open(&Options::from_url(&unknown).unwrap()).await.err().unwrap();
        assert!(e.to_string().contains("does not monitor"), "{e}");
        let e = Connection::open(&Options::from_url("redis+sentinel://127.0.0.1:1/kwmaster").unwrap()).await.err().unwrap();
        assert!(matches!(e, RedisError::Config(_)), "{e}");

        // A real failover. The pool's connection points at the old master;
        // after the switch it notices (READONLY or a closed connection),
        // drops it, and the next command reaches the new master.
        let pool = Pool::from_url(&url).unwrap();
        pool.with(async |c| c.query::<()>(Cmd::new("SET").arg(&key).arg("before failover")).await).await.unwrap();
        let sentinel = url.trim_start_matches("redis+sentinel://").split(',').next().unwrap().to_string();
        let mut s = Connection::open(&Options::from_url(&format!("redis://{sentinel}?protocol=2")).unwrap()).await.unwrap();
        s.query::<()>(Cmd::new("SENTINEL").arg("FAILOVER").arg("kwmaster")).await.unwrap();
        let started = Instant::now();
        while master_port(&url).await == before {
            assert!(started.elapsed() < Duration::from_secs(30), "failover did not happen");
            rt_core::sleep(Duration::from_millis(100)).await;
        }
        let after = master_port(&url).await;
        // Until the old master is reconfigured as a replica it still takes
        // writes (Sentinel's switch-over window); wait for that, then the
        // pool must move on.
        let old_url = format!("redis://127.0.0.1:{before}?protocol=2");
        loop {
            // Demotion closes the node's client connections: ask afresh each time.
            let role = match Connection::open(&Options::from_url(&old_url).unwrap()).await {
                Ok(mut c) => c.query::<Value>(Cmd::new("ROLE")).await.ok(),
                Err(_) => None,
            };
            if role.and_then(|r| r.as_items().and_then(|i| i.first()).and_then(Value::as_text)).as_deref() == Some("slave") {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "the old master was never demoted");
            rt_core::sleep(Duration::from_millis(100)).await;
        }
        let mut written = None;
        for _ in 0..50 {
            match pool.with(async |c| c.query::<()>(Cmd::new("SET").arg(&key).arg("after failover")).await).await {
                Ok(()) => {
                    written = Some(());
                    break;
                }
                // Transient while the old master steps down.
                Err(_) => rt_core::sleep(Duration::from_millis(100)).await,
            }
        }
        assert!(written.is_some(), "the pool never reached the new master");
        let port = pool.with(async |c| Ok::<_, RedisError>(port_of(c).await)).await.unwrap();
        assert_eq!(port, after, "writes now go to the new master");
        pool.with(async |c| c.query::<()>(Cmd::new("DEL").arg(&key)).await).await.unwrap();
    });
}
