//! Typed commands on a [`Connection`]: any command via [`query`], pipelines
//! of [`Cmd`]s, transactions (`WATCH` / `MULTI` / `EXEC`), Lua scripts,
//! cursor scans, blocking pops, and helpers for strings, keys, hashes, lists,
//! sets and sorted sets.
//!
//! [`query`]: Connection::query

use crate::cmd::{pairs, Cmd, FromValue, ToArg};
use crate::conn::Connection;
use crate::error::RedisError;
use crate::resp::Value;
use std::cell::RefCell;
use std::time::Duration;

/// `SET` options.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetOptions {
    /// Expire after this long (`PX`, millisecond precision).
    pub expire: Option<Duration>,
    /// Only set if the key does not exist (`NX`) / only if it does (`XX`).
    pub condition: Option<SetCondition>,
    /// Keep the key's current TTL (`KEEPTTL`).
    pub keep_ttl: bool,
}

/// `SET … NX` / `XX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetCondition {
    /// Only if the key does not exist.
    IfAbsent,
    /// Only if the key exists.
    IfPresent,
}

/// A Lua script, run with `EVALSHA` once the server knows it: the first call
/// (or a server that lost its script cache) loads it with `SCRIPT LOAD`.
pub struct Script {
    source: String,
    sha: RefCell<Option<String>>,
}

impl Script {
    /// A script from its Lua source.
    pub fn new(source: impl Into<String>) -> Self {
        Self { source: source.into(), sha: RefCell::new(None) }
    }

    /// The script's SHA1 once loaded.
    pub fn sha(&self) -> Option<String> {
        self.sha.borrow().clone()
    }

    /// Run with `keys` (`KEYS[i]`) and `args` (`ARGV[i]`).
    pub async fn run<T: FromValue>(&self, c: &mut Connection, keys: &[&str], args: &[&[u8]]) -> Result<T, RedisError> {
        let known = self.sha.borrow().clone();
        if let Some(sha) = known {
            match c.query::<Value>(eval_cmd("EVALSHA", &sha, keys, args)).await {
                Err(RedisError::Server(m)) if m.starts_with("NOSCRIPT") => {}
                other => return T::from_value(other?),
            }
        }
        let sha: String = c.query(Cmd::new("SCRIPT").arg("LOAD").arg(self.source.as_str())).await?;
        *self.sha.borrow_mut() = Some(sha.clone());
        T::from_value(c.query::<Value>(eval_cmd("EVALSHA", &sha, keys, args)).await?)
    }
}

fn eval_cmd(name: &str, script: &str, keys: &[&str], args: &[&[u8]]) -> Cmd {
    Cmd::new(name).arg(script).arg(keys.len()).args(keys.iter().copied()).args(args.iter().copied())
}

/// One page of a cursor scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPage<T> {
    /// Where to continue; `0` once the scan is complete.
    pub cursor: u64,
    /// The items of this page (a page may be empty while the scan goes on).
    pub items: Vec<T>,
}

impl Connection {
    /// Send `cmd` and convert its reply.
    pub async fn query<T: FromValue>(&mut self, cmd: Cmd) -> Result<T, RedisError> {
        let args: Vec<&[u8]> = cmd.args.iter().map(Vec::as_slice).collect();
        T::from_value(self.command(&args).await?)
    }

    /// Send every command, then read every reply in order (one round trip).
    pub async fn query_pipeline(&mut self, cmds: &[Cmd]) -> Result<Vec<Result<Value, RedisError>>, RedisError> {
        let parts: Vec<Vec<Vec<u8>>> = cmds.iter().map(|c| c.args.clone()).collect();
        self.pipeline(&parts).await
    }

    // --- transactions ------------------------------------------------------

    /// `WATCH key…`: the next [`exec`](Self::exec) fails (returns `None`) if
    /// any of them changes first — optimistic locking.
    pub async fn watch(&mut self, keys: &[&str]) -> Result<(), RedisError> {
        self.query(Cmd::new("WATCH").args(keys.iter().copied())).await
    }

    /// `UNWATCH`.
    pub async fn unwatch(&mut self) -> Result<(), RedisError> {
        self.query(Cmd::new("UNWATCH")).await
    }

    /// `MULTI` … `EXEC` in one round trip: the commands run atomically.
    /// `Ok(None)` when a watched key changed (nothing ran — retry);
    /// otherwise each command's own result (one failing at run time, e.g.
    /// `WRONGTYPE`, does not undo the others — Redis has no rollback). A
    /// command refused while queuing (bad arity) aborts the whole
    /// transaction with an `EXECABORT` error.
    pub async fn exec(&mut self, cmds: &[Cmd]) -> Result<Option<Vec<Result<Value, RedisError>>>, RedisError> {
        let mut all = Vec::with_capacity(cmds.len() + 2);
        all.push(Cmd::new("MULTI"));
        all.extend_from_slice(cmds);
        all.push(Cmd::new("EXEC"));
        let mut replies = self.query_pipeline(&all).await?;
        let exec = replies.pop().expect("EXEC reply");
        // A queuing error makes EXEC fail with EXECABORT; report the cause.
        if let Err(RedisError::Server(m)) = &exec {
            let cause = replies.iter().skip(1).find_map(|r| r.as_ref().err().map(ToString::to_string));
            return Err(RedisError::Server(match cause {
                Some(c) => format!("{m} ({c})"),
                None => m.clone(),
            }));
        }
        match exec? {
            Value::Nil => Ok(None),
            Value::Array(items) => Ok(Some(
                items
                    .into_iter()
                    .map(|v| match v {
                        Value::Error(m) => Err(RedisError::Server(m)),
                        v => Ok(v),
                    })
                    .collect(),
            )),
            other => Err(RedisError::unexpected("EXEC", &other)),
        }
    }

    // --- scripting ---------------------------------------------------------

    /// `EVAL script numkeys key… arg…`.
    pub async fn eval<T: FromValue>(&mut self, script: &str, keys: &[&str], args: &[&[u8]]) -> Result<T, RedisError> {
        self.query(eval_cmd("EVAL", script, keys, args)).await
    }

    /// `FCALL function numkeys key… arg…` (Redis 7 functions).
    pub async fn fcall<T: FromValue>(&mut self, function: &str, keys: &[&str], args: &[&[u8]]) -> Result<T, RedisError> {
        self.query(eval_cmd("FCALL", function, keys, args)).await
    }

    // --- scans and blocking commands ----------------------------------------

    /// One page of `SCAN cursor [MATCH pattern] [COUNT n]` (keys).
    pub async fn scan(&mut self, cursor: u64, pattern: Option<&str>, count: Option<usize>) -> Result<ScanPage<String>, RedisError> {
        self.scan_cmd(Cmd::new("SCAN").arg(cursor), pattern, count).await
    }

    /// One page of `HSCAN key cursor …`: field / value pairs.
    pub async fn hscan(&mut self, key: &str, cursor: u64, pattern: Option<&str>, count: Option<usize>) -> Result<ScanPage<(String, Vec<u8>)>, RedisError> {
        let (cursor, v) = self.scan_raw(Cmd::new("HSCAN").arg(key).arg(cursor), pattern, count).await?;
        Ok(ScanPage { cursor, items: pairs(v)? })
    }

    /// One page of `SSCAN key cursor …`.
    pub async fn sscan(&mut self, key: &str, cursor: u64, pattern: Option<&str>, count: Option<usize>) -> Result<ScanPage<String>, RedisError> {
        self.scan_cmd(Cmd::new("SSCAN").arg(key).arg(cursor), pattern, count).await
    }

    /// One page of `ZSCAN key cursor …`: member / score pairs.
    pub async fn zscan(&mut self, key: &str, cursor: u64, pattern: Option<&str>, count: Option<usize>) -> Result<ScanPage<(String, f64)>, RedisError> {
        let (cursor, v) = self.scan_raw(Cmd::new("ZSCAN").arg(key).arg(cursor), pattern, count).await?;
        Ok(ScanPage { cursor, items: pairs(v)? })
    }

    /// Every key matching `pattern`, collected through a full `SCAN` (never
    /// `KEYS`, which blocks the server).
    pub async fn scan_all(&mut self, pattern: &str) -> Result<Vec<String>, RedisError> {
        let mut cursor = 0;
        let mut out = Vec::new();
        loop {
            let page = self.scan(cursor, Some(pattern), Some(1000)).await?;
            out.extend(page.items);
            if page.cursor == 0 {
                return Ok(out);
            }
            cursor = page.cursor;
        }
    }

    async fn scan_cmd(&mut self, cmd: Cmd, pattern: Option<&str>, count: Option<usize>) -> Result<ScanPage<String>, RedisError> {
        let (cursor, v) = self.scan_raw(cmd, pattern, count).await?;
        Ok(ScanPage { cursor, items: Vec::<String>::from_value(v)? })
    }

    async fn scan_raw(&mut self, mut cmd: Cmd, pattern: Option<&str>, count: Option<usize>) -> Result<(u64, Value), RedisError> {
        if let Some(p) = pattern {
            cmd = cmd.arg("MATCH").arg(p);
        }
        if let Some(n) = count {
            cmd = cmd.arg("COUNT").arg(n);
        }
        match self.query::<Value>(cmd).await? {
            Value::Array(mut parts) if parts.len() == 2 => {
                let items = parts.pop().unwrap_or(Value::Nil);
                let cursor = u64::from_value(parts.pop().unwrap_or(Value::Nil))?;
                Ok((cursor, items))
            }
            other => Err(RedisError::unexpected("SCAN", &other)),
        }
    }

    /// Run a command that blocks server-side for up to `wait`, extending the
    /// reply timeout by that much for this call.
    async fn blocking<T: FromValue>(&mut self, cmd: Cmd, wait: Duration) -> Result<T, RedisError> {
        let saved = self.timeout();
        self.set_timeout(saved.map(|t| t + wait));
        let r = self.query(cmd).await;
        self.set_timeout(saved);
        r
    }

    /// `BLPOP key… timeout` → `(key, value)` from the first non-empty list,
    /// or `None` after `wait`.
    pub async fn blpop(&mut self, keys: &[&str], wait: Duration) -> Result<Option<(String, Vec<u8>)>, RedisError> {
        self.blocking(Cmd::new("BLPOP").args(keys.iter().copied()).arg(secs(wait)), wait).await
    }

    /// `BRPOP key… timeout`.
    pub async fn brpop(&mut self, keys: &[&str], wait: Duration) -> Result<Option<(String, Vec<u8>)>, RedisError> {
        self.blocking(Cmd::new("BRPOP").args(keys.iter().copied()).arg(secs(wait)), wait).await
    }

    // --- strings -------------------------------------------------------------

    /// `SET key value [PX ms] [NX|XX] [KEEPTTL]` → whether it was set (a
    /// condition can refuse it).
    pub async fn set(&mut self, key: &str, value: impl ToArg, opts: SetOptions) -> Result<bool, RedisError> {
        let mut cmd = Cmd::new("SET").arg(key).arg(value);
        if let Some(t) = opts.expire {
            cmd = cmd.arg("PX").arg(t.as_millis().max(1) as u64);
        }
        match opts.condition {
            Some(SetCondition::IfAbsent) => cmd = cmd.arg("NX"),
            Some(SetCondition::IfPresent) => cmd = cmd.arg("XX"),
            None => {}
        }
        if opts.keep_ttl {
            cmd = cmd.arg("KEEPTTL");
        }
        self.query(cmd).await
    }

    /// `MGET key…` → the values in key order (`None` for a missing key).
    pub async fn mget(&mut self, keys: &[&str]) -> Result<Vec<Option<Vec<u8>>>, RedisError> {
        self.query(Cmd::new("MGET").args(keys.iter().copied())).await
    }

    /// `MSET key value …` (atomic).
    pub async fn mset(&mut self, pairs: &[(&str, &[u8])]) -> Result<(), RedisError> {
        let mut cmd = Cmd::new("MSET");
        for (k, v) in pairs {
            cmd = cmd.arg(*k).arg(*v);
        }
        self.query(cmd).await
    }

    /// `INCRBY key n` → the new value.
    pub async fn incr_by(&mut self, key: &str, n: i64) -> Result<i64, RedisError> {
        self.query(Cmd::new("INCRBY").arg(key).arg(n)).await
    }

    /// `INCRBYFLOAT key x` → the new value.
    pub async fn incr_by_float(&mut self, key: &str, x: f64) -> Result<f64, RedisError> {
        self.query(Cmd::new("INCRBYFLOAT").arg(key).arg(x)).await
    }

    /// `GETDEL key` → the value, removing the key.
    pub async fn getdel(&mut self, key: &str) -> Result<Option<Vec<u8>>, RedisError> {
        self.query(Cmd::new("GETDEL").arg(key)).await
    }

    // --- keys ----------------------------------------------------------------

    /// `EXISTS key…` → how many of them exist.
    pub async fn exists(&mut self, keys: &[&str]) -> Result<u64, RedisError> {
        self.query(Cmd::new("EXISTS").args(keys.iter().copied())).await
    }

    /// `UNLINK key…` → how many were removed (freed in the background).
    pub async fn unlink(&mut self, keys: &[&str]) -> Result<u64, RedisError> {
        self.query(Cmd::new("UNLINK").args(keys.iter().copied())).await
    }

    /// `PEXPIRE key ms` → whether the key exists.
    pub async fn expire_in(&mut self, key: &str, ttl: Duration) -> Result<bool, RedisError> {
        self.query(Cmd::new("PEXPIRE").arg(key).arg(ttl.as_millis() as u64)).await
    }

    /// `PTTL key` → the remaining time to live; `None` when the key has no
    /// expiry or does not exist.
    pub async fn ttl(&mut self, key: &str) -> Result<Option<Duration>, RedisError> {
        let ms: i64 = self.query(Cmd::new("PTTL").arg(key)).await?;
        Ok((ms >= 0).then(|| Duration::from_millis(ms as u64)))
    }

    /// `PERSIST key` → whether a TTL was removed.
    pub async fn persist(&mut self, key: &str) -> Result<bool, RedisError> {
        self.query(Cmd::new("PERSIST").arg(key)).await
    }

    /// `TYPE key` → `string`, `list`, `set`, `zset`, `hash`, `stream` or `none`.
    pub async fn key_type(&mut self, key: &str) -> Result<String, RedisError> {
        self.query(Cmd::new("TYPE").arg(key)).await
    }

    /// `RENAME key newkey`.
    pub async fn rename(&mut self, key: &str, to: &str) -> Result<(), RedisError> {
        self.query(Cmd::new("RENAME").arg(key).arg(to)).await
    }

    // --- hashes --------------------------------------------------------------

    /// `HSET key field value …` → how many fields were added (not updated).
    pub async fn hset(&mut self, key: &str, fields: &[(&str, &[u8])]) -> Result<u64, RedisError> {
        let mut cmd = Cmd::new("HSET").arg(key);
        for (f, v) in fields {
            cmd = cmd.arg(*f).arg(*v);
        }
        self.query(cmd).await
    }

    /// `HGET key field`.
    pub async fn hget(&mut self, key: &str, field: &str) -> Result<Option<Vec<u8>>, RedisError> {
        self.query(Cmd::new("HGET").arg(key).arg(field)).await
    }

    /// `HGETALL key` → field / value pairs (empty for a missing key).
    pub async fn hgetall(&mut self, key: &str) -> Result<Vec<(String, Vec<u8>)>, RedisError> {
        pairs(self.query::<Value>(Cmd::new("HGETALL").arg(key)).await?)
    }

    /// `HDEL key field…` → how many were removed.
    pub async fn hdel(&mut self, key: &str, fields: &[&str]) -> Result<u64, RedisError> {
        self.query(Cmd::new("HDEL").arg(key).args(fields.iter().copied())).await
    }

    /// `HINCRBY key field n` → the new value.
    pub async fn hincr_by(&mut self, key: &str, field: &str, n: i64) -> Result<i64, RedisError> {
        self.query(Cmd::new("HINCRBY").arg(key).arg(field).arg(n)).await
    }

    // --- lists ---------------------------------------------------------------

    /// `LPUSH key value…` → the new length.
    pub async fn lpush(&mut self, key: &str, values: &[&[u8]]) -> Result<u64, RedisError> {
        self.query(Cmd::new("LPUSH").arg(key).args(values.iter().copied())).await
    }

    /// `RPUSH key value…` → the new length.
    pub async fn rpush(&mut self, key: &str, values: &[&[u8]]) -> Result<u64, RedisError> {
        self.query(Cmd::new("RPUSH").arg(key).args(values.iter().copied())).await
    }

    /// `LPOP key count` → up to `count` values from the head.
    pub async fn lpop(&mut self, key: &str, count: usize) -> Result<Vec<Vec<u8>>, RedisError> {
        self.query(Cmd::new("LPOP").arg(key).arg(count)).await
    }

    /// `LRANGE key start stop` (inclusive, negative from the end).
    pub async fn lrange(&mut self, key: &str, start: i64, stop: i64) -> Result<Vec<Vec<u8>>, RedisError> {
        self.query(Cmd::new("LRANGE").arg(key).arg(start).arg(stop)).await
    }

    /// `LLEN key`.
    pub async fn llen(&mut self, key: &str) -> Result<u64, RedisError> {
        self.query(Cmd::new("LLEN").arg(key)).await
    }

    // --- sets ----------------------------------------------------------------

    /// `SISMEMBER key member`.
    pub async fn sismember(&mut self, key: &str, member: &str) -> Result<bool, RedisError> {
        self.query(Cmd::new("SISMEMBER").arg(key).arg(member)).await
    }

    /// `SINTER key…`.
    pub async fn sinter(&mut self, keys: &[&str]) -> Result<Vec<String>, RedisError> {
        self.query(Cmd::new("SINTER").args(keys.iter().copied())).await
    }

    // --- sorted sets ---------------------------------------------------------

    /// `ZADD key score member` with a fractional score → whether it was added.
    pub async fn zadd_f(&mut self, key: &str, score: f64, member: &str) -> Result<bool, RedisError> {
        self.query(Cmd::new("ZADD").arg(key).arg(score).arg(member)).await
    }

    /// `ZINCRBY key x member` → the new score.
    pub async fn zincr_by(&mut self, key: &str, x: f64, member: &str) -> Result<f64, RedisError> {
        self.query(Cmd::new("ZINCRBY").arg(key).arg(x).arg(member)).await
    }

    /// `ZRANGE key start stop WITHSCORES` (by rank) → member / score pairs.
    pub async fn zrange_with_scores(&mut self, key: &str, start: i64, stop: i64) -> Result<Vec<(String, f64)>, RedisError> {
        pairs(self.query::<Value>(Cmd::new("ZRANGE").arg(key).arg(start).arg(stop).arg("WITHSCORES")).await?)
    }

    /// `ZRANK key member` → the 0-based rank, `None` if absent.
    pub async fn zrank(&mut self, key: &str, member: &str) -> Result<Option<u64>, RedisError> {
        self.query(Cmd::new("ZRANK").arg(key).arg(member)).await
    }

    /// `ZREM key member…` → how many were removed.
    pub async fn zrem(&mut self, key: &str, members: &[&str]) -> Result<u64, RedisError> {
        self.query(Cmd::new("ZREM").arg(key).args(members.iter().copied())).await
    }

    /// `ZCARD key`.
    pub async fn zcard(&mut self, key: &str) -> Result<u64, RedisError> {
        self.query(Cmd::new("ZCARD").arg(key)).await
    }

    // --- server --------------------------------------------------------------

    /// `INFO [section]` as `key → value` pairs (sections flattened).
    pub async fn info(&mut self, section: Option<&str>) -> Result<Vec<(String, String)>, RedisError> {
        let mut cmd = Cmd::new("INFO");
        if let Some(s) = section {
            cmd = cmd.arg(s);
        }
        let text: String = self.query(cmd).await?;
        Ok(text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .filter_map(|l| l.split_once(':').map(|(k, v)| (k.to_string(), v.trim_end().to_string())))
            .collect())
    }

    /// `DBSIZE` → keys in the selected database.
    pub async fn dbsize(&mut self) -> Result<u64, RedisError> {
        self.query(Cmd::new("DBSIZE")).await
    }
}

/// A blocking timeout in seconds (Redis takes a decimal; 0 would block forever).
fn secs(d: Duration) -> String {
    let s = d.as_secs_f64().max(0.001);
    format!("{s:.3}")
}
