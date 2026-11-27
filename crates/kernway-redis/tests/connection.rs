//! The connection layer against a real Redis (`KERNWAY_REDIS_URL`, e.g.
//! `redis://127.0.0.1:6380`): URL options, RESP3 / RESP2, authentication,
//! database selection, client names, server errors that keep the connection
//! in step, timeouts that drop it, pipelining, RESP3 pushes, pub/sub with
//! patterns, large and binary values, and the pool's handling of all that.

use kernway_redis::{Connection, Options, Pool, Protocol, RedisError, Value};
use std::time::{Duration, Instant};

fn url() -> Option<String> {
    std::env::var("KERNWAY_REDIS_URL").ok()
}

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

macro_rules! need_redis {
    () => {
        match url() {
            Some(u) => u,
            None => return eprintln!("skip: KERNWAY_REDIS_URL not set"),
        }
    };
}

/// A unique key prefix per test, so tests can run in parallel.
fn key(test: &str, k: &str) -> String {
    format!("kwtest:{test}:{}:{k}", std::process::id())
}

async fn open(url: &str) -> Connection {
    Connection::open(&Options::from_url(url).unwrap()).await.unwrap()
}

#[test]
fn resp3_is_negotiated_and_resp2_on_request() {
    let url = need_redis!();
    run(async move {
        let mut c = open(&url).await;
        assert_eq!(c.protocol(), Protocol::Resp3);
        // RESP3 types come back typed.
        let k = key("resp3", "h");
        c.command(&[b"HSET", k.as_bytes(), b"a", b"1", b"b", b"2"]).await.unwrap();
        assert!(matches!(c.command(&[b"HGETALL", k.as_bytes()]).await.unwrap(), Value::Map(m) if m.len() == 2));
        c.command(&[b"ZADD", k.as_bytes(), b"1.5", b"m"]).await.err(); // WRONGTYPE: still fine below
        let z = key("resp3", "z");
        c.command(&[b"ZADD", z.as_bytes(), b"1.5", b"m"]).await.unwrap();
        assert_eq!(c.command(&[b"ZSCORE", z.as_bytes(), b"m"]).await.unwrap(), Value::Double(1.5));
        c.command(&[b"DEL", k.as_bytes(), z.as_bytes()]).await.unwrap();

        let mut c2 = Connection::open(&Options::from_url(&format!("{url}?protocol=2")).unwrap().protocol(Protocol::Resp2)).await.unwrap();
        assert_eq!(c2.protocol(), Protocol::Resp2);
        c2.command(&[b"ZADD", z.as_bytes(), b"1.5", b"m"]).await.unwrap();
        assert_eq!(c2.command(&[b"ZSCORE", z.as_bytes(), b"m"]).await.unwrap(), Value::Bulk(b"1.5".to_vec()));
        c2.command(&[b"DEL", z.as_bytes()]).await.unwrap();
    });
}

#[test]
fn a_server_error_leaves_the_connection_in_step() {
    let url = need_redis!();
    run(async move {
        for proto in ["2", "3"] {
            let mut c = open(&format!("{url}?protocol={proto}")).await;
            let k = key("err", proto);
            c.command(&[b"SET", k.as_bytes(), b"text"]).await.unwrap();
            // Wrong type, unknown command, wrong arity — each an error…
            assert!(matches!(c.command(&[b"LPUSH", k.as_bytes(), b"x"]).await, Err(RedisError::Server(m)) if m.starts_with("WRONGTYPE")));
            assert!(matches!(c.command(&[b"NOSUCHCOMMAND"]).await, Err(RedisError::Server(_))));
            assert!(matches!(c.command(&[b"GET"]).await, Err(RedisError::Server(_))));
            // …and the very next reply is the right one.
            assert_eq!(c.command(&[b"GET", k.as_bytes()]).await.unwrap(), Value::Bulk(b"text".to_vec()));
            assert_eq!(c.command(&[b"ECHO", b"after"]).await.unwrap(), Value::Bulk(b"after".to_vec()));
            assert!(!c.is_broken());
            c.command(&[b"DEL", k.as_bytes()]).await.unwrap();
        }
    });
}

#[test]
fn acl_users_passwords_databases_and_client_names() {
    let url = need_redis!();
    run(async move {
        let mut admin = open(&url).await;
        let user = format!("kwuser{}", std::process::id());
        admin
            .command(&[b"ACL", b"SETUSER", user.as_bytes(), b"on", ">p@ss w0rd/é".as_bytes(), b"~*", b"&*", b"+@all"])
            .await
            .unwrap();
        let hostport = url.trim_start_matches("redis://").split(['/', '?']).next().unwrap().to_string();
        // Percent-encoded password in the URL, RESP3 (HELLO … AUTH) and RESP2 (AUTH).
        for proto in ["3", "2"] {
            let u = format!("redis://{user}:p%40ss%20w0rd%2F%C3%A9@{hostport}/5?protocol={proto}&client_name=kw-{proto}");
            let mut c = open(&u).await;
            assert_eq!(c.command(&[b"ACL", b"WHOAMI"]).await.unwrap().as_text().unwrap(), user);
            assert_eq!(c.command(&[b"CLIENT", b"GETNAME"]).await.unwrap().as_text().unwrap(), format!("kw-{proto}"));
            let info = c.command(&[b"CLIENT", b"INFO"]).await.unwrap().as_text().unwrap();
            assert!(info.contains(" db=5 "), "{info}");
        }
        // Database isolation.
        let k = key("db", "k");
        let mut d5 = open(&format!("{url}/5")).await;
        let mut d0 = open(&url).await;
        d5.command(&[b"SET", k.as_bytes(), b"in five"]).await.unwrap();
        assert_eq!(d0.command(&[b"EXISTS", k.as_bytes()]).await.unwrap(), Value::Int(0));
        assert_eq!(d5.command(&[b"GET", k.as_bytes()]).await.unwrap(), Value::Bulk(b"in five".to_vec()));
        d5.command(&[b"DEL", k.as_bytes()]).await.unwrap();
        // A wrong password is a server error at open, for both protocols.
        for proto in ["3", "2"] {
            let bad = Connection::open(&Options::from_url(&format!("redis://{user}:wrong@{hostport}?protocol={proto}")).unwrap()).await;
            assert!(matches!(bad, Err(RedisError::Server(ref m)) if m.contains("WRONGPASS")), "{proto}: {:?}", bad.err());
        }
        admin.command(&[b"ACL", b"DELUSER", user.as_bytes()]).await.unwrap();
    });
}

#[test]
fn host_names_resolve_off_the_reactor() {
    let url = need_redis!();
    run(async move {
        let named = url.replace("127.0.0.1", "localhost");
        let mut c = open(&named).await;
        assert_eq!(c.command(&[b"PING"]).await.unwrap(), Value::Simple("PONG".into()));
        let e = Connection::open(&Options::from_url("redis://no-such-host.invalid:6379").unwrap()).await.err().unwrap();
        assert!(matches!(e, RedisError::Config(ref m) if m.contains("resolve")), "{e}");
    });
}

#[test]
fn a_reply_timeout_drops_the_connection_and_the_pool_replaces_it() {
    let url = need_redis!();
    run(async move {
        let opts = Options::from_url(&format!("{url}?timeout=200")).unwrap();
        let mut c = Connection::open(&opts).await.unwrap();
        let started = Instant::now();
        // BLPOP on an empty list blocks 2 s server-side: our timeout fires first.
        let e = c.command(&[b"BLPOP", key("timeout", "never").as_bytes(), b"2"]).await.unwrap_err();
        assert!(matches!(e, RedisError::Timeout(_)), "{e}");
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(c.is_broken(), "the late BLPOP reply would desync it");
        assert!(matches!(c.command(&[b"PING"]).await, Err(RedisError::Closed)));

        let pool = Pool::from_options(opts);
        let r = pool.with(async |c| c.command(&[b"BLPOP", key("timeout", "never2").as_bytes(), b"2"]).await).await;
        assert!(matches!(r, Err(RedisError::Timeout(_))));
        // The broken connection was not pooled: the next command works.
        let pong = pool.with(async |c| c.command(&[b"PING"]).await).await.unwrap();
        assert_eq!(pong, Value::Simple("PONG".into()));
    });
}

#[test]
fn connect_timeout() {
    run(async {
        // A non-routable address: the SYN goes nowhere.
        let opts = Options::from_url("redis://10.255.255.1:6379?connect_timeout=300").unwrap();
        let started = Instant::now();
        let e = Connection::open(&opts).await.err().expect("must not connect");
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        assert!(matches!(e, RedisError::Timeout(_) | RedisError::Io(_)), "{e}");
    });
}

#[test]
fn the_pool_keeps_a_connection_after_a_server_error() {
    let url = need_redis!();
    run(async move {
        let pool = Pool::from_url(&url).unwrap();
        let id1 = pool.with(async |c| c.command(&[b"CLIENT", b"ID"]).await).await.unwrap();
        assert!(pool.with(async |c| c.command(&[b"NOSUCHCOMMAND"]).await).await.is_err());
        let id2 = pool.with(async |c| c.command(&[b"CLIENT", b"ID"]).await).await.unwrap();
        assert_eq!(id1, id2, "same connection reused after an error reply");
    });
}

#[test]
fn pipelines_send_everything_then_read_in_order() {
    let url = need_redis!();
    run(async move {
        let mut c = open(&url).await;
        let k = key("pipe", "n");
        let mut cmds: Vec<Vec<Vec<u8>>> = vec![vec![b"DEL".to_vec(), k.as_bytes().to_vec()]];
        for _ in 0..1000 {
            cmds.push(vec![b"INCR".to_vec(), k.as_bytes().to_vec()]);
        }
        cmds.push(vec![b"LPUSH".to_vec(), k.as_bytes().to_vec(), b"x".to_vec()]); // WRONGTYPE
        cmds.push(vec![b"GET".to_vec(), k.as_bytes().to_vec()]);
        let replies = c.pipeline(&cmds).await.unwrap();
        assert_eq!(replies.len(), 1003);
        assert_eq!(replies[1000].as_ref().unwrap(), &Value::Int(1000));
        assert!(matches!(&replies[1001], Err(RedisError::Server(m)) if m.starts_with("WRONGTYPE")));
        assert_eq!(replies[1002].as_ref().unwrap(), &Value::Bulk(b"1000".to_vec()));
        c.command(&[b"DEL", k.as_bytes()]).await.unwrap();
    });
}

#[test]
fn large_and_binary_values_round_trip() {
    let url = need_redis!();
    run(async move {
        let mut c = open(&url).await;
        let k = key("big", "v");
        let big: Vec<u8> = (0..12_000_000u32).map(|i| (i % 251) as u8).collect();
        c.command(&[b"SET", k.as_bytes(), &big]).await.unwrap();
        assert_eq!(c.command(&[b"STRLEN", k.as_bytes()]).await.unwrap(), Value::Int(12_000_000));
        assert_eq!(c.command(&[b"GET", k.as_bytes()]).await.unwrap().as_bulk().unwrap(), &big[..]);
        let odd = b"\r\n$3\r\n*1\r\n\x00\xff";
        c.command(&[b"SET", k.as_bytes(), odd]).await.unwrap();
        assert_eq!(c.command(&[b"GET", k.as_bytes()]).await.unwrap(), Value::Bulk(odd.to_vec()));
        let vk = key("big", "Việt ✓");
        c.command(&[b"SET", vk.as_bytes(), "giá trị 😀".as_bytes()]).await.unwrap();
        assert_eq!(c.command(&[b"GET", vk.as_bytes()]).await.unwrap().as_text().unwrap(), "giá trị 😀");
        c.command(&[b"DEL", k.as_bytes(), vk.as_bytes()]).await.unwrap();
    });
}

#[test]
fn resp3_pushes_are_queued_not_mistaken_for_replies() {
    let url = need_redis!();
    run(async move {
        let mut c = open(&url).await;
        let k = key("track", "k");
        c.command(&[b"SET", k.as_bytes(), b"1"]).await.unwrap();
        // Client-side caching: reading a key subscribes to its invalidation.
        c.command(&[b"CLIENT", b"TRACKING", b"ON"]).await.unwrap();
        c.command(&[b"GET", k.as_bytes()]).await.unwrap();
        let mut other = open(&url).await;
        other.command(&[b"SET", k.as_bytes(), b"2"]).await.unwrap();
        // The invalidation push arrives before this reply; the reply is still right.
        assert_eq!(c.command(&[b"ECHO", b"x"]).await.unwrap(), Value::Bulk(b"x".to_vec()));
        let push = c.next_push().await.unwrap();
        assert_eq!(push[0], Value::Bulk(b"invalidate".to_vec()));
        let keys: Vec<String> = push[1].as_items().unwrap().iter().filter_map(Value::as_text).collect();
        assert_eq!(keys, std::slice::from_ref(&k), "the invalidated key");
        c.command(&[b"DEL", k.as_bytes()]).await.unwrap();
    });
}

#[test]
fn pubsub_channels_and_patterns_on_both_protocols() {
    let url = need_redis!();
    run(async move {
        for proto in ["2", "3"] {
            let ch = key("ps", &format!("news.{proto}"));
            let pat = key("ps", "news.*");
            let mut sub = open(&format!("{url}?protocol={proto}")).await;
            sub.subscribe(&[&ch]).await.unwrap();
            sub.psubscribe(&[&pat]).await.unwrap();
            assert!(sub.is_subscribed());
            let mut publisher = open(&url).await;
            assert_eq!(publisher.command(&[b"PUBLISH", ch.as_bytes(), b"hello"]).await.unwrap(), Value::Int(2), "channel + pattern");
            let mut got = [sub.next_pubsub().await.unwrap(), sub.next_pubsub().await.unwrap()];
            got.sort_by_key(|m| m.pattern.is_some());
            assert_eq!((got[0].pattern.as_deref(), got[0].channel.as_str(), &got[0].payload[..]), (None, ch.as_str(), &b"hello"[..]));
            assert_eq!(got[1].pattern.as_deref(), Some(pat.as_str()));
            sub.unsubscribe(&[]).await.unwrap();
            sub.punsubscribe(&[]).await.unwrap();
            assert!(!sub.is_subscribed(), "{proto}: back to normal mode");
            assert_eq!(sub.command(&[b"PING"]).await.unwrap(), Value::Simple("PONG".into()));
        }
    });
}

#[test]
fn bad_endpoints_fail_clearly() {
    run(async {
        // Nothing listens on port 1.
        let e = Connection::open(&Options::from_url("redis://127.0.0.1:1").unwrap()).await.err().unwrap();
        assert!(matches!(e, RedisError::Io(_)), "{e}");
        #[cfg(not(unix))]
        {
            let e = Connection::open(&Options::unix("C:/redis.sock")).await.err().unwrap();
            assert!(matches!(e, RedisError::Config(_)));
        }
    });
}

/// Over the server's Unix socket (`KERNWAY_REDIS_SOCKET`): run inside the
/// Redis container by the acceptance script, and on Linux CI.
#[cfg(unix)]
#[test]
fn unix_socket_connects_with_options() {
    let Ok(path) = std::env::var("KERNWAY_REDIS_SOCKET") else {
        return eprintln!("skip: KERNWAY_REDIS_SOCKET not set");
    };
    run(async move {
        let mut c = open(&format!("redis+unix://{path}?db=3&client_name=kw-unix")).await;
        assert_eq!(c.protocol(), Protocol::Resp3);
        let info = c.command(&[b"CLIENT", b"INFO"]).await.unwrap().as_text().unwrap();
        assert!(info.contains(" db=3 ") && info.contains("name=kw-unix"), "{info}");
        // The server sees a Unix-socket client (no TCP peer address).
        assert!(info.contains("laddr=/") || info.contains("addr=/"), "{info}");
        assert_eq!(c.command(&[b"PING"]).await.unwrap(), Value::Simple("PONG".into()));
    });
}
