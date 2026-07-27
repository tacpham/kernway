//! One Redis connection: a Kernway `AsyncTcpStream` plus a read buffer, speaking
//! RESP one request/reply at a time.
//!
//! `command` is the whole surface — write the request, read one reply. The typed
//! helpers ([`get`](Connection::get), [`set_ex`](Connection::set_ex), …) are thin
//! wrappers over it, for the handful of commands the session store needs; anything
//! else goes through `command` directly. The connection is a request/reply pipe,
//! not a pool — sharing across cores is [`Pool`](crate::Pool)'s job.

use std::net::SocketAddr;

use rt_net::AsyncTcpStream;

use crate::error::RedisError;
use crate::resp::{self, Value};

/// A single, exclusively-owned connection to a Redis server.
pub struct Connection {
    stream: AsyncTcpStream,
    /// Bytes read from the socket but not yet parsed into a reply.
    buf: Vec<u8>,
    /// How far into `buf` the parser has consumed.
    pos: usize,
}

impl Connection {
    /// Open a connection to `addr`. Sets `TCP_NODELAY` — request/reply latency
    /// matters more than batching for a session store.
    pub async fn connect(addr: SocketAddr) -> Result<Self, RedisError> {
        let stream = AsyncTcpStream::connect(addr).await?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            buf: Vec::with_capacity(4096),
            pos: 0,
        })
    }

    /// Send one command (RESP array of bulk strings) and read one reply. A `-`
    /// error reply comes back as [`RedisError::Server`].
    pub async fn command(&mut self, args: &[&[u8]]) -> Result<Value, RedisError> {
        let mut out = Vec::with_capacity(32);
        resp::encode(args, &mut out);
        self.stream.write_all(&out).await?;
        self.read_reply().await
    }

    /// Parse the next reply, reading from the socket until a whole one arrives.
    async fn read_reply(&mut self) -> Result<Value, RedisError> {
        loop {
            if self.pos < self.buf.len() {
                if let Some((value, used)) = resp::parse(&self.buf[self.pos..])? {
                    self.pos += used;
                    // A request/reply client fully drains the buffer each time;
                    // reset so it does not grow without bound.
                    if self.pos == self.buf.len() {
                        self.buf.clear();
                        self.pos = 0;
                    }
                    return Ok(value);
                }
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(RedisError::Closed);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    // --- typed helpers, for the commands the session store issues ----------

    /// `PING` — a liveness / handshake check.
    pub async fn ping(&mut self) -> Result<(), RedisError> {
        self.command(&[b"PING"]).await?;
        Ok(())
    }

    /// `AUTH [user] password` — authenticate the connection. `user` is `None` for
    /// a password-only server, `Some(_)` for an ACL user. Sent once per physical
    /// connection right after connect (the [`Pool`](crate::Pool) does this).
    pub async fn auth(&mut self, user: Option<&str>, password: &str) -> Result<(), RedisError> {
        match user {
            Some(user) => {
                self.command(&[b"AUTH", user.as_bytes(), password.as_bytes()])
                    .await?
            }
            None => self.command(&[b"AUTH", password.as_bytes()]).await?,
        };
        Ok(())
    }

    /// `GET key` → the value, or `None` if the key is absent.
    pub async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, RedisError> {
        match self.command(&[b"GET", key.as_bytes()]).await? {
            Value::Bulk(bytes) => Ok(Some(bytes)),
            Value::Nil => Ok(None),
            other => Err(RedisError::unexpected("GET", &other)),
        }
    }

    /// `SET key value EX ttl` — store with an expiry (seconds).
    pub async fn set_ex(
        &mut self,
        key: &str,
        value: &[u8],
        ttl_secs: u64,
    ) -> Result<(), RedisError> {
        let ttl = ttl_secs.to_string();
        self.command(&[b"SET", key.as_bytes(), value, b"EX", ttl.as_bytes()])
            .await?;
        Ok(())
    }

    /// `DEL key…` → the number of keys removed.
    pub async fn del(&mut self, keys: &[&str]) -> Result<i64, RedisError> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
        args.push(b"DEL");
        args.extend(keys.iter().map(|k| k.as_bytes()));
        Ok(self.command(&args).await?.as_int().unwrap_or(0))
    }

    /// `SADD key member` — add to a set (used to index a user's sessions).
    pub async fn sadd(&mut self, key: &str, member: &str) -> Result<(), RedisError> {
        self.command(&[b"SADD", key.as_bytes(), member.as_bytes()])
            .await?;
        Ok(())
    }

    /// `SREM key member` — remove from a set.
    pub async fn srem(&mut self, key: &str, member: &str) -> Result<(), RedisError> {
        self.command(&[b"SREM", key.as_bytes(), member.as_bytes()])
            .await?;
        Ok(())
    }

    /// `SMEMBERS key` → every member, as strings (non-UTF-8 members are skipped).
    pub async fn smembers(&mut self, key: &str) -> Result<Vec<String>, RedisError> {
        match self.command(&[b"SMEMBERS", key.as_bytes()]).await? {
            Value::Array(items) => Ok(items
                .into_iter()
                .filter_map(|v| match v {
                    Value::Bulk(b) => String::from_utf8(b).ok(),
                    _ => None,
                })
                .collect()),
            Value::Nil => Ok(Vec::new()),
            other => Err(RedisError::unexpected("SMEMBERS", &other)),
        }
    }

    /// `EXPIRE key ttl` — (re)set a key's TTL in seconds.
    pub async fn expire(&mut self, key: &str, ttl_secs: u64) -> Result<(), RedisError> {
        let ttl = ttl_secs.to_string();
        self.command(&[b"EXPIRE", key.as_bytes(), ttl.as_bytes()])
            .await?;
        Ok(())
    }

    /// `SCARD key` → the number of members in a set (0 for a missing key).
    pub async fn scard(&mut self, key: &str) -> Result<i64, RedisError> {
        Ok(self
            .command(&[b"SCARD", key.as_bytes()])
            .await?
            .as_int()
            .unwrap_or(0))
    }

    // --- sorted sets (for presence: score = last-heartbeat timestamp) -------

    /// `ZADD key score member` — add or update a member's score.
    pub async fn zadd(&mut self, key: &str, score: i64, member: &str) -> Result<(), RedisError> {
        let score = score.to_string();
        self.command(&[b"ZADD", key.as_bytes(), score.as_bytes(), member.as_bytes()])
            .await?;
        Ok(())
    }

    /// `ZSCORE key member` → the member's score, or `None` if it is not present.
    pub async fn zscore(&mut self, key: &str, member: &str) -> Result<Option<i64>, RedisError> {
        match self
            .command(&[b"ZSCORE", key.as_bytes(), member.as_bytes()])
            .await?
        {
            Value::Bulk(bytes) => Ok(std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.parse().ok())),
            Value::Nil => Ok(None),
            other => Err(RedisError::unexpected("ZSCORE", &other)),
        }
    }

    /// `ZRANGEBYSCORE key min max` → members whose score is in `[min, max]`, as
    /// strings. `min`/`max` are the raw Redis bounds (`"-inf"`, `"+inf"`, a number,
    /// or `"(1700"` for exclusive).
    pub async fn zrangebyscore(
        &mut self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<Vec<String>, RedisError> {
        match self
            .command(&[
                b"ZRANGEBYSCORE",
                key.as_bytes(),
                min.as_bytes(),
                max.as_bytes(),
            ])
            .await?
        {
            Value::Array(items) => Ok(items
                .into_iter()
                .filter_map(|v| match v {
                    Value::Bulk(b) => String::from_utf8(b).ok(),
                    _ => None,
                })
                .collect()),
            Value::Nil => Ok(Vec::new()),
            other => Err(RedisError::unexpected("ZRANGEBYSCORE", &other)),
        }
    }

    /// `ZCOUNT key min max` → how many members score within `[min, max]`.
    pub async fn zcount(&mut self, key: &str, min: &str, max: &str) -> Result<i64, RedisError> {
        Ok(self
            .command(&[b"ZCOUNT", key.as_bytes(), min.as_bytes(), max.as_bytes()])
            .await?
            .as_int()
            .unwrap_or(0))
    }

    /// `ZREMRANGEBYSCORE key min max` → drop members scoring within `[min, max]`
    /// (prunes stale heartbeats). Returns how many were removed.
    pub async fn zremrangebyscore(
        &mut self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<i64, RedisError> {
        Ok(self
            .command(&[
                b"ZREMRANGEBYSCORE",
                key.as_bytes(),
                min.as_bytes(),
                max.as_bytes(),
            ])
            .await?
            .as_int()
            .unwrap_or(0))
    }
}
