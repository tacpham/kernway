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
use crate::stream::{self, StreamEntries, StreamEntry};

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

    // --- pub/sub ------------------------------------------------------------
    //
    // `command`'s send-one/read-one shape fits every request/reply command, but a
    // subscriber is different: after `SUBSCRIBE` the server pushes `message` frames
    // whenever a publisher sends, with no request in between. So subscribing is split
    // into `subscribe` (enter the mode) + `next_message` (block for the next push).

    /// `PUBLISH channel payload` → the number of subscribers that received it.
    /// **Fire-and-forget**: with no subscriber connected at publish time the message is
    /// dropped — Redis pub/sub is not durable, so a subscriber that was offline must
    /// reconcile any missed work from a durable source on reconnect.
    pub async fn publish(&mut self, channel: &str, payload: &[u8]) -> Result<i64, RedisError> {
        Ok(self
            .command(&[b"PUBLISH", channel.as_bytes(), payload])
            .await?
            .as_int()
            .unwrap_or(0))
    }

    /// `SUBSCRIBE channel…` — put this connection into subscribe mode and drain the
    /// per-channel confirmation frames. After this the connection is **dedicated** to
    /// receiving pushes via [`next_message`](Self::next_message); in RESP2 only
    /// (un)subscribe and PING are valid on a subscribed connection, so it must not be
    /// returned to a [`Pool`](crate::Pool) — keep it for the lifetime of the listener.
    pub async fn subscribe(&mut self, channels: &[&str]) -> Result<(), RedisError> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(channels.len() + 1);
        args.push(b"SUBSCRIBE");
        args.extend(channels.iter().map(|c| c.as_bytes()));
        let mut out = Vec::with_capacity(32);
        resp::encode(&args, &mut out);
        self.stream.write_all(&out).await?;
        // One `subscribe` confirmation frame per channel.
        for _ in 0..channels.len() {
            self.read_reply().await?;
        }
        Ok(())
    }

    /// Block until the next `message` on a subscribed channel, returning
    /// `(channel, payload)`. It awaits the socket, so this call *is* the push wake-up —
    /// it returns the instant a publisher sends, without polling. Frames that are not a
    /// message (extra subscribe confirmations, a `pong` from a keepalive PING) are
    /// skipped. Requires a prior [`subscribe`](Self::subscribe).
    pub async fn next_message(&mut self) -> Result<(String, Vec<u8>), RedisError> {
        loop {
            if let Value::Array(items) = self.read_reply().await? {
                // A published message is exactly `["message", channel, payload]`.
                if let [Value::Bulk(kind), Value::Bulk(channel), Value::Bulk(payload)] =
                    items.as_slice()
                {
                    if kind.as_slice() == b"message" {
                        return Ok((
                            String::from_utf8_lossy(channel).into_owned(),
                            payload.clone(),
                        ));
                    }
                }
            }
            // Any other frame (subscribe/unsubscribe/pong) → keep waiting for a message.
        }
    }

    // --- streams (a durable queue with consumer groups) ----------------------
    //
    // Unlike pub/sub, a stream entry read through a consumer group stays in the
    // group's pending list until `XACK`ed, so a crashed consumer loses nothing: its
    // entries are recovered by another consumer with `xautoclaim`. Reply parsing
    // lives in [`crate::stream`].

    /// `XADD key [MAXLEN ~ n] id field value …` → the new entry's ID. `id` is
    /// usually `"*"` (server-assigned). `maxlen_approx` caps the stream at about
    /// `n` entries (`MAXLEN ~`, which trims whole macro-nodes — cheap).
    pub async fn xadd(
        &mut self,
        key: &str,
        id: &str,
        fields: &[(&str, &[u8])],
        maxlen_approx: Option<u64>,
    ) -> Result<String, RedisError> {
        let args = stream::xadd_args(key, id, fields, maxlen_approx);
        match self.command(&stream::as_slices(&args)).await? {
            Value::Bulk(b) => String::from_utf8(b)
                .map_err(|_| RedisError::Protocol("XADD: non-UTF-8 entry id".into())),
            other => Err(RedisError::unexpected("XADD", &other)),
        }
    }

    /// `XGROUP CREATE key group start_id [MKSTREAM]` → `true` if created, `false`
    /// if the group already existed (`BUSYGROUP`), so it is safe to call on every
    /// startup. `start_id` is `"$"` (only new entries) or `"0"` (whole history);
    /// `mkstream` creates an empty stream if the key does not exist.
    pub async fn xgroup_create(
        &mut self,
        key: &str,
        group: &str,
        start_id: &str,
        mkstream: bool,
    ) -> Result<bool, RedisError> {
        let mut args: Vec<&[u8]> = vec![
            b"XGROUP",
            b"CREATE",
            key.as_bytes(),
            group.as_bytes(),
            start_id.as_bytes(),
        ];
        if mkstream {
            args.push(b"MKSTREAM");
        }
        match self.command(&args).await {
            Ok(_) => Ok(true),
            Err(e) if stream::is_busygroup(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// `XREADGROUP GROUP group consumer [COUNT n] [BLOCK ms] STREAMS key… id…` →
    /// the entries delivered, per stream. Use id `">"` for never-delivered entries,
    /// or `"0"` to re-read this consumer's own pending ones. A `BLOCK` that times
    /// out returns an empty vec. Note `BLOCK` holds this connection for the wait.
    pub async fn xreadgroup(
        &mut self,
        group: &str,
        consumer: &str,
        streams: &[(&str, &str)],
        count: Option<usize>,
        block_ms: Option<u64>,
    ) -> Result<Vec<StreamEntries>, RedisError> {
        let args = stream::xreadgroup_args(group, consumer, streams, count, block_ms);
        let reply = self.command(&stream::as_slices(&args)).await?;
        stream::parse_read_reply(&reply)
    }

    /// `XACK key group id…` → how many entries were acknowledged (removed from the
    /// group's pending list).
    pub async fn xack(&mut self, key: &str, group: &str, ids: &[&str]) -> Result<i64, RedisError> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(ids.len() + 3);
        args.extend([b"XACK".as_slice(), key.as_bytes(), group.as_bytes()]);
        args.extend(ids.iter().map(|id| id.as_bytes()));
        match self.command(&args).await? {
            Value::Int(n) => Ok(n),
            other => Err(RedisError::unexpected("XACK", &other)),
        }
    }

    /// `XLEN key` → the number of entries (0 for a missing key).
    pub async fn xlen(&mut self, key: &str) -> Result<i64, RedisError> {
        match self.command(&[b"XLEN", key.as_bytes()]).await? {
            Value::Int(n) => Ok(n),
            other => Err(RedisError::unexpected("XLEN", &other)),
        }
    }

    /// `XPENDING key group` (summary form) → the total number of entries delivered
    /// to the group but not yet acknowledged.
    pub async fn xpending_count(&mut self, key: &str, group: &str) -> Result<i64, RedisError> {
        let reply = self
            .command(&[b"XPENDING", key.as_bytes(), group.as_bytes()])
            .await?;
        stream::parse_pending_count(&reply)
    }

    /// `XAUTOCLAIM key group consumer min_idle_ms start COUNT count` → transfer
    /// entries pending for at least `min_idle_ms` (their consumer is presumed dead)
    /// to `consumer`, returning `(next start, claimed entries)`. Call again with the
    /// returned start until it is `"0-0"` to sweep the whole pending list.
    pub async fn xautoclaim(
        &mut self,
        key: &str,
        group: &str,
        consumer: &str,
        min_idle_ms: u64,
        start: &str,
        count: usize,
    ) -> Result<(String, Vec<StreamEntry>), RedisError> {
        let idle = min_idle_ms.to_string();
        let count = count.to_string();
        let reply = self
            .command(&[
                b"XAUTOCLAIM",
                key.as_bytes(),
                group.as_bytes(),
                consumer.as_bytes(),
                idle.as_bytes(),
                start.as_bytes(),
                b"COUNT",
                count.as_bytes(),
            ])
            .await?;
        stream::parse_autoclaim_reply(&reply)
    }

    /// `XTRIM key MAXLEN [~] n` → how many entries were evicted. `approx` (`~`)
    /// lets Redis trim only whole macro-nodes, which is much cheaper.
    pub async fn xtrim_maxlen(
        &mut self,
        key: &str,
        n: u64,
        approx: bool,
    ) -> Result<i64, RedisError> {
        let n = n.to_string();
        let mut args: Vec<&[u8]> = vec![b"XTRIM", key.as_bytes(), b"MAXLEN"];
        if approx {
            args.push(b"~");
        }
        args.push(n.as_bytes());
        match self.command(&args).await? {
            Value::Int(n) => Ok(n),
            other => Err(RedisError::unexpected("XTRIM", &other)),
        }
    }
}
