//! Typed commands against a real Redis (`KERNWAY_REDIS_URL`): any command via
//! `query`, transactions with optimistic locking, Lua scripts, scans,
//! blocking pops, and the helpers for every data type — each on RESP3 and
//! RESP2, since replies differ in shape between them.

use kernway_redis::{Cmd, Connection, Options, RedisError, Script, SetCondition, SetOptions, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

fn url() -> Option<String> {
    std::env::var("KERNWAY_REDIS_URL").ok()
}

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

/// Run `body` on a RESP3 and a RESP2 connection.
fn both<F>(test: &'static str, body: F)
where
    F: for<'a> Fn(&'a mut Connection, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
{
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_REDIS_URL not set") };
    for proto in ["3", "2"] {
        let url = url.clone();
        run(async {
            let mut c = Connection::open(&Options::from_url(&format!("{url}?protocol={proto}")).unwrap()).await.unwrap();
            let prefix = format!("kwcmd:{test}:{}:{proto}:", std::process::id());
            body(&mut c, prefix.clone()).await;
            let keys = c.scan_all(&format!("{prefix}*")).await.unwrap();
            if !keys.is_empty() {
                let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
                c.unlink(&refs).await.unwrap();
            }
        });
    }
}

#[test]
fn any_command_typed_through_query() {
    both("query", |c, p| {
        Box::pin(async move {
            let k = format!("{p}n");
            let n: i64 = c.query(Cmd::new("INCRBY").arg(&k).arg(5)).await.unwrap();
            assert_eq!(n, 5);
            let s: String = c.query(Cmd::new("GET").arg(&k)).await.unwrap();
            assert_eq!(s, "5");
            let missing: Option<Vec<u8>> = c.query(Cmd::new("GET").arg(format!("{p}none"))).await.unwrap();
            assert_eq!(missing, None);
            let b: bool = c.query(Cmd::new("EXISTS").arg(&k)).await.unwrap();
            assert!(b);
            let h = format!("{p}h");
            c.hset(&h, &[("a", b"1"), ("b", b"2")]).await.unwrap();
            let m: HashMap<String, i64> = c.query(Cmd::new("HGETALL").arg(&h)).await.unwrap();
            assert_eq!(m, HashMap::from([("a".to_string(), 1), ("b".to_string(), 2)]));
            // A reply that does not fit the requested type is a clear error.
            assert!(matches!(c.query::<i64>(Cmd::new("HGETALL").arg(&h)).await, Err(RedisError::Unexpected(_))));
        })
    });
}

#[test]
fn strings_keys_and_expiry() {
    both("strings", |c, p| {
        Box::pin(async move {
            let k = format!("{p}s");
            assert!(c.set(&k, "v1", SetOptions::default()).await.unwrap());
            let nx = SetOptions { condition: Some(SetCondition::IfAbsent), ..Default::default() };
            assert!(!c.set(&k, "v2", nx).await.unwrap(), "NX refuses an existing key");
            let xx = SetOptions { condition: Some(SetCondition::IfPresent), expire: Some(Duration::from_secs(60)), ..Default::default() };
            assert!(c.set(&k, "v3", xx).await.unwrap());
            let ttl = c.ttl(&k).await.unwrap().unwrap();
            assert!(ttl > Duration::from_secs(55) && ttl <= Duration::from_secs(60), "{ttl:?}");
            let keep = SetOptions { keep_ttl: true, ..Default::default() };
            c.set(&k, "v4", keep).await.unwrap();
            assert!(c.ttl(&k).await.unwrap().is_some(), "KEEPTTL");
            assert!(c.persist(&k).await.unwrap());
            assert_eq!(c.ttl(&k).await.unwrap(), None);
            assert!(c.expire_in(&k, Duration::from_millis(1500)).await.unwrap());
            assert_eq!(c.key_type(&k).await.unwrap(), "string");

            let (a, b, z) = (format!("{p}a"), format!("{p}b"), format!("{p}zz"));
            c.mset(&[(a.as_str(), b"1"), (b.as_str(), b"2")]).await.unwrap();
            assert_eq!(c.mget(&[&a, &z, &b]).await.unwrap(), [Some(b"1".to_vec()), None, Some(b"2".to_vec())]);
            assert_eq!(c.incr_by(&a, 41).await.unwrap(), 42);
            assert_eq!(c.incr_by_float(&b, 0.5).await.unwrap(), 2.5);
            assert_eq!(c.exists(&[&a, &b, &z]).await.unwrap(), 2);
            c.rename(&a, &z).await.unwrap();
            assert_eq!(c.getdel(&z).await.unwrap(), Some(b"42".to_vec()));
            assert_eq!(c.exists(&[&z]).await.unwrap(), 0);
            assert!(matches!(c.rename(&format!("{p}nothing"), &z).await, Err(RedisError::Server(_))));
        })
    });
}

#[test]
fn hashes_lists_sets_and_sorted_sets() {
    both("types", |c, p| {
        Box::pin(async move {
            let h = format!("{p}h");
            assert_eq!(c.hset(&h, &[("name", "Lan".as_bytes()), ("visits", b"1")]).await.unwrap(), 2);
            assert_eq!(c.hincr_by(&h, "visits", 2).await.unwrap(), 3);
            assert_eq!(c.hget(&h, "name").await.unwrap(), Some(b"Lan".to_vec()));
            let mut all = c.hgetall(&h).await.unwrap();
            all.sort();
            assert_eq!(all, [("name".to_string(), b"Lan".to_vec()), ("visits".to_string(), b"3".to_vec())]);
            assert_eq!(c.hdel(&h, &["name", "nope"]).await.unwrap(), 1);
            assert!(c.hgetall(&format!("{p}none")).await.unwrap().is_empty());

            let l = format!("{p}l");
            c.rpush(&l, &[b"a", b"b", b"c"]).await.unwrap();
            c.lpush(&l, &[b"z"]).await.unwrap();
            assert_eq!(c.lrange(&l, 0, -1).await.unwrap(), [b"z".to_vec(), b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
            assert_eq!(c.lpop(&l, 2).await.unwrap(), [b"z".to_vec(), b"a".to_vec()]);
            assert_eq!(c.llen(&l).await.unwrap(), 2);
            assert!(c.lpop(&format!("{p}none"), 1).await.unwrap().is_empty());

            let (s1, s2) = (format!("{p}s1"), format!("{p}s2"));
            for m in ["a", "b", "c"] {
                c.sadd(&s1, m).await.unwrap();
            }
            for m in ["b", "c", "d"] {
                c.sadd(&s2, m).await.unwrap();
            }
            assert!(c.sismember(&s1, "a").await.unwrap());
            let mut inter = c.sinter(&[&s1, &s2]).await.unwrap();
            inter.sort();
            assert_eq!(inter, ["b", "c"]);
            let mut members = c.smembers(&s1).await.unwrap();
            members.sort();
            assert_eq!(members, ["a", "b", "c"]);

            let z = format!("{p}z");
            assert!(c.zadd_f(&z, 1.5, "lan").await.unwrap());
            c.zadd_f(&z, -2.0, "mai").await.unwrap();
            c.zadd_f(&z, f64::INFINITY, "top").await.unwrap();
            assert_eq!(c.zincr_by(&z, 1.0, "lan").await.unwrap(), 2.5);
            assert_eq!(
                c.zrange_with_scores(&z, 0, -1).await.unwrap(),
                [("mai".to_string(), -2.0), ("lan".to_string(), 2.5), ("top".to_string(), f64::INFINITY)]
            );
            assert_eq!(c.zrank(&z, "lan").await.unwrap(), Some(1));
            assert_eq!(c.zrank(&z, "nobody").await.unwrap(), None);
            assert_eq!(c.zrem(&z, &["top"]).await.unwrap(), 1);
            assert_eq!(c.zcard(&z).await.unwrap(), 2);
            // The original integer-score helpers still work on both protocols.
            c.zadd(&z, 7, "int").await.unwrap();
            assert_eq!(c.zscore(&z, "int").await.unwrap(), Some(7));
        })
    });
}

#[test]
fn transactions_are_atomic_and_watch_detects_races() {
    both("tx", |c, p| {
        Box::pin(async move {
            let (a, b) = (format!("{p}a"), format!("{p}b"));
            let r = c.exec(&[Cmd::new("SET").arg(&a).arg(1), Cmd::new("INCR").arg(&a), Cmd::new("GET").arg(&a)]).await.unwrap().unwrap();
            assert_eq!(r.len(), 3);
            assert_eq!(r[2].as_ref().unwrap().as_text().unwrap(), "2");

            // A run-time error fails that command only (Redis has no rollback).
            let r = c.exec(&[Cmd::new("SET").arg(&b).arg("x"), Cmd::new("INCR").arg(&b), Cmd::new("SET").arg(&a).arg(9)]).await.unwrap().unwrap();
            assert!(r[0].is_ok() && r[2].is_ok());
            assert!(matches!(&r[1], Err(RedisError::Server(m)) if m.contains("not an integer")));
            // A queuing error aborts it all.
            let e = c.exec(&[Cmd::new("SET").arg(&a).arg(10), Cmd::new("GET")]).await.unwrap_err();
            assert!(matches!(&e, RedisError::Server(m) if m.starts_with("EXECABORT")), "{e}");
            assert_eq!(c.get(&a).await.unwrap(), Some(b"9".to_vec()), "nothing of the aborted transaction ran");

            // WATCH: another client changes the key → EXEC returns None.
            let url = url().unwrap();
            let mut other = Connection::open(&Options::from_url(&url).unwrap()).await.unwrap();
            c.watch(&[&a]).await.unwrap();
            other.query::<()>(Cmd::new("SET").arg(&a).arg(100)).await.unwrap();
            assert!(c.exec(&[Cmd::new("SET").arg(&a).arg(1)]).await.unwrap().is_none(), "a watched key changed");
            assert_eq!(c.get(&a).await.unwrap(), Some(b"100".to_vec()));
            // Unchanged → it runs.
            c.watch(&[&a]).await.unwrap();
            assert!(c.exec(&[Cmd::new("INCR").arg(&a)]).await.unwrap().is_some());
            c.watch(&[&a]).await.unwrap();
            c.unwatch().await.unwrap();
            other.query::<()>(Cmd::new("SET").arg(&a).arg(5)).await.unwrap();
            assert!(c.exec(&[Cmd::new("INCR").arg(&a)]).await.unwrap().is_some(), "UNWATCH released it");
        })
    });
}

#[test]
fn lua_scripts_load_once_and_survive_a_script_flush() {
    both("lua", |c, p| {
        Box::pin(async move {
            let k = format!("{p}k");
            let n: i64 = c.eval("return redis.call('INCRBY', KEYS[1], ARGV[1])", &[&k], &[b"3"]).await.unwrap();
            assert_eq!(n, 3);
            let script = Script::new("local v = redis.call('INCRBY', KEYS[1], ARGV[1]); return {v, ARGV[2]}");
            let (v, tag): (i64, String) = script.run(c, &[&k], &[b"10", "nhãn".as_bytes()]).await.unwrap();
            assert_eq!((v, tag.as_str()), (13, "nhãn"));
            let sha = script.sha().unwrap();
            assert_eq!(sha.len(), 40);
            // The server forgets every script: the next run reloads it.
            c.query::<()>(Cmd::new("SCRIPT").arg("FLUSH")).await.unwrap();
            let (v, _): (i64, String) = script.run(c, &[&k], &[b"1", b"x"]).await.unwrap();
            assert_eq!(v, 14);
            // A script error is a server error; the connection goes on.
            assert!(matches!(c.eval::<Value>("return redis.call('NOPE')", &[], &[]).await, Err(RedisError::Server(_))));
            assert_eq!(c.incr_by(&k, 0).await.unwrap(), 14);
        })
    });
}

#[test]
fn scans_walk_everything_in_pages() {
    both("scan", |c, p| {
        Box::pin(async move {
            for i in 0..250 {
                c.query::<()>(Cmd::new("SET").arg(format!("{p}k{i}")).arg(i)).await.unwrap();
            }
            let all = c.scan_all(&format!("{p}k*")).await.unwrap();
            assert_eq!(all.len(), 250);
            let h = format!("{p}h");
            let fields: Vec<(String, Vec<u8>)> = (0..300).map(|i| (format!("f{i}"), i.to_string().into_bytes())).collect();
            let refs: Vec<(&str, &[u8])> = fields.iter().map(|(f, v)| (f.as_str(), v.as_slice())).collect();
            c.hset(&h, &refs).await.unwrap();
            let (mut cursor, mut seen) = (0, 0);
            loop {
                let page = c.hscan(&h, cursor, None, Some(50)).await.unwrap();
                seen += page.items.len();
                cursor = page.cursor;
                if cursor == 0 {
                    break;
                }
            }
            assert_eq!(seen, 300);
            let z = format!("{p}z");
            c.zadd_f(&z, 0.5, "a").await.unwrap();
            assert_eq!(c.zscan(&z, 0, None, None).await.unwrap().items, [("a".to_string(), 0.5)]);
            let s = format!("{p}s");
            c.sadd(&s, "m").await.unwrap();
            assert_eq!(c.sscan(&s, 0, Some("m*"), None).await.unwrap().items, ["m"]);
        })
    });
}

#[test]
fn blocking_pops_wait_and_extend_the_reply_timeout() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_REDIS_URL not set") };
    run(async move {
        let k = format!("kwcmd:block:{}", std::process::id());
        // A 200 ms reply timeout, a 600 ms blocking pop: the pop's own wait is added.
        let mut c = Connection::open(&Options::from_url(&format!("{url}?timeout=200")).unwrap()).await.unwrap();
        let started = Instant::now();
        assert_eq!(c.blpop(&[&k], Duration::from_millis(600)).await.unwrap(), None);
        assert!(started.elapsed() >= Duration::from_millis(550), "{:?}", started.elapsed());
        assert!(!c.is_broken());
        assert_eq!(c.timeout(), Some(Duration::from_millis(200)), "restored");
        // A value pushed meanwhile by someone else wakes it.
        let pusher = {
            let url = url.clone();
            let k = k.clone();
            std::thread::spawn(move || {
                run(async move {
                    rt_core::sleep(Duration::from_millis(100)).await;
                    let mut p = Connection::open(&Options::from_url(&url).unwrap()).await.unwrap();
                    p.rpush(&k, &[b"job"]).await.unwrap();
                })
            })
        };
        let got = c.brpop(&[&k], Duration::from_secs(3)).await.unwrap();
        assert_eq!(got, Some((k.clone(), b"job".to_vec())));
        pusher.join().unwrap();
    });
}

#[test]
fn server_info_and_sizes() {
    both("info", |c, _p| {
        Box::pin(async move {
            let info = c.info(Some("server")).await.unwrap();
            assert!(info.iter().any(|(k, v)| k == "redis_version" && !v.is_empty()));
            let _ = c.dbsize().await.unwrap();
        })
    });
}
