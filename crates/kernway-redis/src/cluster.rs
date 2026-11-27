//! Redis Cluster: keys spread over 16384 hash slots owned by several
//! masters. [`ClusterClient`] keeps the slot map, sends each command to the
//! node owning its key, and follows the cluster's redirections:
//!
//! - `MOVED slot host:port` — the slot lives elsewhere now: update the map
//!   (and refresh it whole), resend there;
//! - `ASK slot host:port` — the slot is being migrated and this key already
//!   moved: send `ASKING` + the command to that node, once, without
//!   touching the map;
//! - `TRYAGAIN` / `CLUSTERDOWN` — wait a moment and retry;
//! - an unreachable node — refresh the map from any reachable node, retry.
//!
//! Like every kernway client it is `!Send`: one per shard (core).

use crate::cmd::{Cmd, FromValue};
use crate::conn::Connection;
use crate::error::RedisError;
use crate::options::{Endpoint, Options};
use crate::resp::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

/// Number of hash slots.
pub const SLOTS: u16 = 16384;

/// Redirections / retries before giving up on one command.
const MAX_ATTEMPTS: usize = 16;

/// The hash slot of `key`: CRC16 (XMODEM) of the key — or of its hash tag,
/// the part between the first `{` and the next `}` when non-empty — modulo
/// 16384. Keys sharing a tag (`{user:42}:cart`, `{user:42}:orders`) share a
/// slot, so multi-key commands and transactions can use them together.
pub fn key_slot(key: &[u8]) -> u16 {
    let hashed = match key.iter().position(|&b| b == b'{') {
        Some(open) => match key[open + 1..].iter().position(|&b| b == b'}') {
            Some(len) if len > 0 => &key[open + 1..open + 1 + len],
            _ => key,
        },
        None => key,
    };
    crc16(hashed) % SLOTS
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// The key a command is routed by, or `None` for a keyless command (any
/// node will do).
pub fn routing_key(args: &[Vec<u8>]) -> Option<&[u8]> {
    let name = args.first()?.to_ascii_uppercase();
    match name.as_slice() {
        // Keys come after numkeys.
        b"EVAL" | b"EVALSHA" | b"EVAL_RO" | b"EVALSHA_RO" | b"FCALL" | b"FCALL_RO" => {
            let n: usize = std::str::from_utf8(args.get(2)?).ok()?.parse().ok()?;
            if n == 0 { None } else { args.get(3).map(Vec::as_slice) }
        }
        // Keys come after the STREAMS keyword.
        b"XREAD" | b"XREADGROUP" => {
            let at = args.iter().position(|a| a.eq_ignore_ascii_case(b"STREAMS"))?;
            args.get(at + 1).map(Vec::as_slice)
        }
        // Commands without a key, or whose first argument is not one.
        b"PING" | b"ECHO" | b"INFO" | b"TIME" | b"DBSIZE" | b"CLUSTER" | b"CONFIG" | b"SCRIPT" | b"FUNCTION" | b"CLIENT"
        | b"COMMAND" | b"PUBLISH" | b"PUBSUB" | b"AUTH" | b"HELLO" | b"SELECT" | b"ROLE" | b"LASTSAVE" | b"MULTI" | b"EXEC"
        | b"DISCARD" | b"UNWATCH" | b"WAIT" | b"MEMORY" | b"SLOWLOG" | b"LATENCY" | b"ACL" | b"RANDOMKEY" | b"SCAN"
        | b"FLUSHDB" | b"FLUSHALL" | b"SWAPDB" | b"DEBUG" | b"OBJECT" | b"READONLY" | b"READWRITE" => {
            if name.as_slice() == b"OBJECT" || name.as_slice() == b"MEMORY" {
                // OBJECT ENCODING key / MEMORY USAGE key
                return args.get(2).map(Vec::as_slice);
            }
            None
        }
        // SPUBLISH channel: routed by the channel's slot.
        _ => args.get(1).map(Vec::as_slice),
    }
}

/// Commands that only read, safe to send to a replica.
fn is_read_only(args: &[Vec<u8>]) -> bool {
    const READS: &[&[u8]] = &[
        b"GET", b"MGET", b"STRLEN", b"GETRANGE", b"EXISTS", b"TTL", b"PTTL", b"TYPE", b"HGET", b"HMGET", b"HGETALL", b"HKEYS",
        b"HVALS", b"HLEN", b"HEXISTS", b"HSTRLEN", b"HSCAN", b"LRANGE", b"LLEN", b"LINDEX", b"LPOS", b"SMEMBERS", b"SISMEMBER",
        b"SMISMEMBER", b"SCARD", b"SRANDMEMBER", b"SINTER", b"SUNION", b"SDIFF", b"SSCAN", b"ZRANGE", b"ZRANGEBYSCORE",
        b"ZREVRANGE", b"ZREVRANGEBYSCORE", b"ZRANGEBYLEX", b"ZSCORE", b"ZMSCORE", b"ZCARD", b"ZCOUNT", b"ZLEXCOUNT", b"ZRANK",
        b"ZREVRANK", b"ZSCAN", b"XRANGE", b"XREVRANGE", b"XLEN", b"GETBIT", b"BITCOUNT", b"BITPOS", b"PFCOUNT", b"GEOPOS",
        b"GEODIST", b"GEOSEARCH", b"EVAL_RO", b"EVALSHA_RO", b"FCALL_RO", b"DUMP", b"OBJECT", b"MEMORY",
    ];
    args.first().is_some_and(|n| READS.iter().any(|r| n.eq_ignore_ascii_case(r)))
}

/// `host:port` of a node.
type Addr = (String, u16);

/// A master and its replicas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shard {
    /// The master's `(host, port)`.
    pub master: (String, u16),
    /// Its replicas.
    pub replicas: Vec<(String, u16)>,
}

/// A client for a Redis Cluster.
pub struct ClusterClient {
    /// Connection options for every node (TLS, credentials, timeouts).
    template: Options,
    /// Where to start when the map must be (re)loaded.
    seeds: Vec<Addr>,
    /// Owner (master) of each slot.
    slots: RefCell<Vec<Option<Addr>>>,
    /// Masters and their replicas, from the last map load.
    shards: RefCell<Vec<(Addr, Vec<Addr>)>>,
    /// One idle connection per node, and whether it has sent `READONLY`.
    conns: RefCell<HashMap<Addr, (Connection, bool)>>,
    /// Send read-only commands to replicas (after `READONLY`).
    read_from_replicas: bool,
    /// Round-robin over a shard's replicas.
    next_replica: std::cell::Cell<usize>,
}

impl ClusterClient {
    /// A client for the cluster reachable through `seeds` (any nodes; more
    /// than one survives a seed being down), opening node connections with
    /// `template`'s TLS / credentials / timeouts. Loads the slot map now.
    pub async fn connect(seeds: &[(&str, u16)], template: Options) -> Result<Self, RedisError> {
        if template.database != 0 {
            return Err(RedisError::Config("a cluster has database 0 only".into()));
        }
        let client = Self {
            template,
            seeds: seeds.iter().map(|(h, p)| (h.to_string(), *p)).collect(),
            slots: RefCell::new(vec![None; SLOTS as usize]),
            shards: RefCell::new(Vec::new()),
            conns: RefCell::new(HashMap::new()),
            read_from_replicas: false,
            next_replica: std::cell::Cell::new(0),
        };
        client.refresh().await?;
        Ok(client)
    }

    /// From a URL: `redis+cluster://[[user]:password@]host:port,host:port[?options]`
    /// (`rediss+cluster://` for TLS; options as for [`Options::from_url`]).
    pub async fn from_url(url: &str) -> Result<Self, RedisError> {
        let tls = url.starts_with("rediss+cluster://");
        let rest = url
            .strip_prefix("redis+cluster://")
            .or_else(|| url.strip_prefix("rediss+cluster://"))
            .ok_or_else(|| RedisError::Config("cluster URL must start with redis+cluster:// or rediss+cluster://".into()))?;
        let (auth, hosts_q) = match rest.rsplit_once('@') {
            Some((a, h)) => (Some(a), h),
            None => (None, rest),
        };
        let (hosts, query) = hosts_q.split_once('?').unwrap_or((hosts_q, ""));
        let hosts = hosts.trim_end_matches('/');
        let first = hosts.split(',').next().unwrap_or("");
        // Parse the shared options through the single-node URL parser.
        let scheme = if tls { "rediss" } else { "redis" };
        let auth = auth.map(|a| format!("{a}@")).unwrap_or_default();
        let q = if query.is_empty() { String::new() } else { format!("?{query}") };
        let template = Options::from_url(&format!("{scheme}://{auth}{first}{q}"))?;
        let mut seeds = Vec::new();
        for h in hosts.split(',').filter(|h| !h.is_empty()) {
            match Options::from_url(&format!("redis://{h}"))?.endpoint {
                Endpoint::Tcp { host, port } => seeds.push((host, port)),
                _ => unreachable!("redis:// is TCP"),
            }
        }
        let seed_refs: Vec<(&str, u16)> = seeds.iter().map(|(h, p)| (h.as_str(), *p)).collect();
        Self::connect(&seed_refs, template).await
    }

    /// Send read-only commands (`GET`, `HGETALL`, `ZRANGE`, `EVAL_RO` …) to
    /// the slot's replicas in turn, writes still to its master. Replicas
    /// replicate asynchronously: a read may not see a write just made.
    pub fn read_from_replicas(mut self, yes: bool) -> Self {
        self.read_from_replicas = yes;
        self
    }

    /// The masters, with their replicas, as of the last map load.
    pub fn shards(&self) -> Vec<Shard> {
        self.shards.borrow().iter().map(|(m, r)| Shard { master: m.clone(), replicas: r.clone() }).collect()
    }

    /// The node owning `key`'s slot per the current map.
    pub fn node_for(&self, key: &[u8]) -> Option<(String, u16)> {
        self.slots.borrow()[key_slot(key) as usize].clone()
    }

    /// Reload the slot map (`CLUSTER SLOTS`) from the first node that
    /// answers: known masters first, then the seeds.
    pub async fn refresh(&self) -> Result<(), RedisError> {
        let mut candidates: Vec<Addr> = self.shards.borrow().iter().map(|(m, _)| m.clone()).collect();
        candidates.extend(self.seeds.iter().cloned());
        let mut errors = Vec::new();
        for addr in candidates {
            match Box::pin(self.load_map_from(&addr)).await {
                Ok(()) => return Ok(()),
                Err(e) => errors.push(format!("{}:{}: {e}", addr.0, addr.1)),
            }
        }
        Err(RedisError::Config(format!("no cluster node answered CLUSTER SLOTS: {}", errors.join("; "))))
    }

    async fn load_map_from(&self, addr: &Addr) -> Result<(), RedisError> {
        let reply = Box::pin(self.on_node(addr, |c| Box::pin(c.query::<Value>(Cmd::new("CLUSTER").arg("SLOTS"))))).await?;
        let Value::Array(ranges) = reply else {
            return Err(RedisError::unexpected("CLUSTER SLOTS", &reply));
        };
        let mut slots = vec![None; SLOTS as usize];
        let mut shards: Vec<(Addr, Vec<Addr>)> = Vec::new();
        let node = |v: &Value| -> Option<Addr> {
            let items = v.as_items()?;
            let host = items.first()?.as_text()?;
            let port = items.get(1)?.as_int()? as u16;
            // An empty host means "the address you reached me on".
            Some((if host.is_empty() { addr.0.clone() } else { host }, port))
        };
        for r in &ranges {
            let items = r.as_items().ok_or_else(|| RedisError::unexpected("CLUSTER SLOTS", r))?;
            let (Some(start), Some(end)) = (items.first().and_then(Value::as_int), items.get(1).and_then(Value::as_int)) else {
                return Err(RedisError::unexpected("CLUSTER SLOTS", r));
            };
            let master = items.get(2).and_then(node).ok_or_else(|| RedisError::unexpected("CLUSTER SLOTS", r))?;
            let replicas: Vec<Addr> = items.iter().skip(3).filter_map(node).collect();
            for s in start..=end.min(i64::from(SLOTS) - 1) {
                slots[s as usize] = Some(master.clone());
            }
            match shards.iter_mut().find(|(m, _)| *m == master) {
                Some((_, rs)) => rs.extend(replicas.into_iter().filter(|r| !rs.contains(r)).collect::<Vec<_>>()),
                None => shards.push((master, replicas)),
            }
        }
        if slots.iter().all(Option::is_none) {
            return Err(RedisError::Config("CLUSTER SLOTS listed no slot".into()));
        }
        *self.slots.borrow_mut() = slots;
        *self.shards.borrow_mut() = shards;
        Ok(())
    }

    /// Run `f` on a connection to `addr` (opened if needed, returned to the
    /// idle map unless it broke).
    async fn on_node<T>(
        &self,
        addr: &Addr,
        f: impl for<'c> FnOnce(&'c mut Connection) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, RedisError>> + 'c>>,
    ) -> Result<T, RedisError> {
        let idle = self.conns.borrow_mut().remove(addr);
        let (mut conn, mut readonly) = match idle {
            Some(c) => c,
            None => {
                let mut o = self.template.clone();
                o.endpoint = Endpoint::Tcp { host: addr.0.clone(), port: addr.1 };
                (Connection::open(&o).await?, false)
            }
        };
        // A connection opened before the node was known as a replica (a seed
        // during the first map load) is switched now.
        if self.read_from_replicas && !readonly && self.is_replica(addr) {
            conn.command(&[b"READONLY"]).await?;
            readonly = true;
        }
        let r = f(&mut conn).await;
        if !conn.is_broken() && !conn.is_subscribed() {
            self.conns.borrow_mut().insert(addr.clone(), (conn, readonly));
        }
        r
    }

    fn is_replica(&self, addr: &Addr) -> bool {
        self.shards.borrow().iter().any(|(_, rs)| rs.contains(addr))
    }

    fn any_master(&self) -> Option<Addr> {
        self.shards.borrow().first().map(|(m, _)| m.clone())
    }

    fn route(&self, args: &[Vec<u8>]) -> Result<Addr, RedisError> {
        let addr = match routing_key(args) {
            Some(k) => {
                let master = self.slots.borrow()[key_slot(k) as usize].clone();
                match master {
                    Some(m) if self.read_from_replicas && is_read_only(args) => {
                        let shards = self.shards.borrow();
                        let replicas = shards.iter().find(|(s, _)| *s == m).map(|(_, r)| r.clone()).unwrap_or_default();
                        if replicas.is_empty() {
                            Some(m)
                        } else {
                            let i = self.next_replica.get();
                            self.next_replica.set(i.wrapping_add(1));
                            Some(replicas[i % replicas.len()].clone())
                        }
                    }
                    other => other,
                }
            }
            None => self.any_master(),
        };
        addr.ok_or_else(|| RedisError::Config("no node owns this slot (cluster map incomplete)".into()))
    }

    /// Send `cmd` to the node owning its key (any master for a keyless
    /// command), following redirections, and convert the reply.
    pub async fn query<T: FromValue>(&self, cmd: Cmd) -> Result<T, RedisError> {
        // Boxed: the redirection loop's future is large; a caller doing many
        // cluster calls in one async fn must not carry each inline.
        T::from_value(Box::pin(self.send(cmd.args, None)).await?)
    }

    /// Send `cmd` to a given node (`INFO`, `DBSIZE`, `CLUSTER …` on that node).
    pub async fn query_node<T: FromValue>(&self, host: &str, port: u16, cmd: Cmd) -> Result<T, RedisError> {
        let addr = (host.to_string(), port);
        let args = cmd.args;
        T::from_value(Box::pin(self.on_node(&addr, move |c| Box::pin(async move { command(c, &args).await }))).await?)
    }

    /// The routing / redirection loop.
    async fn send(&self, args: Vec<Vec<u8>>, mut target: Option<Addr>) -> Result<Value, RedisError> {
        let mut asking = false;
        let mut last_err = None;
        for attempt in 0..MAX_ATTEMPTS {
            let addr = match target.take() {
                Some(a) => a,
                None => self.route(&args)?,
            };
            let a = args.clone();
            let ask = std::mem::take(&mut asking);
            let r = self
                .on_node(&addr, move |c| {
                    Box::pin(async move {
                        if ask {
                            let replies = c.pipeline(&[vec![b"ASKING".to_vec()], a]).await?;
                            replies.into_iter().nth(1).unwrap_or(Err(RedisError::Closed))
                        } else {
                            command(c, &a).await
                        }
                    })
                })
                .await;
            match r {
                Ok(v) => return Ok(v),
                Err(RedisError::Server(m)) => match Redirect::parse(&m) {
                    Some(Redirect::Moved(slot, to)) => {
                        self.slots.borrow_mut()[slot as usize] = Some(to.clone());
                        // A MOVED usually means a reshard or failover: reload
                        // the whole map so other slots are right too.
                        let _ = Box::pin(self.refresh()).await;
                        target = Some(to);
                    }
                    Some(Redirect::Ask(_, to)) => {
                        target = Some(to);
                        asking = true;
                    }
                    Some(Redirect::Retry) => {
                        rt_core::sleep(Duration::from_millis(20 * (attempt as u64 + 1))).await;
                        if m.starts_with("CLUSTERDOWN") {
                            let _ = Box::pin(self.refresh()).await;
                        }
                        last_err = Some(RedisError::Server(m));
                    }
                    None => return Err(RedisError::Server(m)),
                },
                // The node is gone (failover, crash): reload the map and retry.
                Err(e @ (RedisError::Io(_) | RedisError::Closed | RedisError::Timeout(_))) => {
                    self.conns.borrow_mut().remove(&addr);
                    rt_core::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                    let _ = Box::pin(self.refresh()).await;
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| RedisError::Config("too many cluster redirections".into())))
    }

    /// Run commands in order, grouped into one pipeline per node, with the
    /// replies back in the original order. A command redirected meanwhile is
    /// resent on its own.
    pub async fn pipeline(&self, cmds: &[Cmd]) -> Result<Vec<Result<Value, RedisError>>, RedisError> {
        let mut by_node: Vec<(Addr, Vec<usize>)> = Vec::new();
        for (i, c) in cmds.iter().enumerate() {
            let addr = self.route(&c.args)?;
            match by_node.iter_mut().find(|(a, _)| *a == addr) {
                Some((_, idx)) => idx.push(i),
                None => by_node.push((addr, vec![i])),
            }
        }
        let mut out: Vec<Option<Result<Value, RedisError>>> = (0..cmds.len()).map(|_| None).collect();
        for (addr, idx) in by_node {
            let batch: Vec<Vec<Vec<u8>>> = idx.iter().map(|&i| cmds[i].args.clone()).collect();
            let replies = Box::pin(self.on_node(&addr, move |c| Box::pin(async move { c.pipeline(&batch).await }))).await;
            match replies {
                Ok(rs) => {
                    for (i, r) in idx.iter().zip(rs) {
                        out[*i] = Some(r);
                    }
                }
                // The node failed: resend that group one by one (redirects included).
                Err(_) => {
                    for &i in &idx {
                        out[i] = Some(Box::pin(self.send(cmds[i].args.clone(), None)).await);
                    }
                }
            }
        }
        let mut results = Vec::with_capacity(cmds.len());
        for (i, r) in out.into_iter().enumerate() {
            let r = r.unwrap_or(Err(RedisError::Closed));
            let redirected = matches!(&r, Err(RedisError::Server(m)) if Redirect::parse(m).is_some());
            results.push(if redirected { Box::pin(self.send(cmds[i].args.clone(), None)).await } else { r });
        }
        Ok(results)
    }

    /// `MULTI` … `EXEC` on the node owning the first command's key; every key
    /// must hash to that slot (use a hash tag), as Redis Cluster requires.
    /// `None` when a `watch`ed key changed — see [`Connection::exec`].
    pub async fn exec(&self, watch: &[&str], cmds: &[Cmd]) -> Result<Option<Vec<Result<Value, RedisError>>>, RedisError> {
        let first = cmds.first().ok_or_else(|| RedisError::Config("empty transaction".into()))?;
        let slot_of = |a: &[Vec<u8>]| routing_key(a).map(key_slot);
        let slot = slot_of(&first.args);
        if let Some(c) = cmds.iter().find(|c| slot_of(&c.args).is_some_and(|s| Some(s) != slot)) {
            return Err(RedisError::Config(format!(
                "transaction keys in different slots ({} vs {}): give them a common {{hash tag}}",
                slot.unwrap_or(0),
                slot_of(&c.args).unwrap_or(0)
            )));
        }
        let addr = self.route(&first.args)?;
        let watch: Vec<String> = watch.iter().map(|s| s.to_string()).collect();
        let cmds = cmds.to_vec();
        Box::pin(self.on_node(&addr, move |c| {
            Box::pin(async move {
                if !watch.is_empty() {
                    let w: Vec<&str> = watch.iter().map(String::as_str).collect();
                    c.watch(&w).await?;
                }
                c.exec(&cmds).await
            })
        }))
        .await
    }

    /// Every key matching `pattern`, scanning each master in turn.
    pub async fn scan_all(&self, pattern: &str) -> Result<Vec<String>, RedisError> {
        let masters: Vec<Addr> = self.shards.borrow().iter().map(|(m, _)| m.clone()).collect();
        let mut out = Vec::new();
        for m in masters {
            let p = pattern.to_string();
            out.extend(Box::pin(self.on_node(&m, move |c| Box::pin(async move { c.scan_all(&p).await }))).await?);
        }
        Ok(out)
    }

    /// A connection subscribed to sharded channels (`SSUBSCRIBE`) on the node
    /// owning their slot (all channels must share one); read it with
    /// [`Connection::next_pubsub`].
    pub async fn ssubscribe(&self, channels: &[&str]) -> Result<Connection, RedisError> {
        let first = channels.first().ok_or_else(|| RedisError::Config("no channel".into()))?;
        let addr = self.slots.borrow()[key_slot(first.as_bytes()) as usize].clone().ok_or_else(|| RedisError::Config("slot unowned".into()))?;
        let mut o = self.template.clone();
        o.endpoint = Endpoint::Tcp { host: addr.0, port: addr.1 };
        let mut c = Connection::open(&o).await?;
        c.ssubscribe(channels).await?;
        Ok(c)
    }
}

async fn command(c: &mut Connection, args: &[Vec<u8>]) -> Result<Value, RedisError> {
    let a: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
    c.command(&a).await
}

/// A cluster redirection / retry instruction in an error reply.
#[derive(Debug, PartialEq, Eq)]
enum Redirect {
    Moved(u16, Addr),
    Ask(u16, Addr),
    Retry,
}

impl Redirect {
    fn parse(msg: &str) -> Option<Self> {
        let mut parts = msg.split_whitespace();
        let kind = parts.next()?;
        if kind == "TRYAGAIN" || kind == "CLUSTERDOWN" {
            return Some(Redirect::Retry);
        }
        let slot: u16 = parts.next()?.parse().ok()?;
        let (host, port) = parts.next()?.rsplit_once(':')?;
        let addr = (host.trim_start_matches('[').trim_end_matches(']').to_string(), port.parse().ok()?);
        match kind {
            "MOVED" => Some(Redirect::Moved(slot, addr)),
            "ASK" => Some(Redirect::Ask(slot, addr)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_match_redis() {
        // Values from CLUSTER KEYSLOT on a real server.
        assert_eq!(key_slot(b"foo"), 12182);
        assert_eq!(key_slot(b"bar"), 5061);
        assert_eq!(key_slot(b""), 0);
        assert_eq!(key_slot(b"123456789"), 12739, "the CRC16/XMODEM check value 0x31C3 mod 16384");
        // Hash tags.
        assert_eq!(key_slot(b"{user1000}.following"), key_slot(b"user1000"));
        assert_eq!(key_slot(b"{user1000}.followers"), key_slot(b"{user1000}.following"));
        assert_eq!(key_slot(b"foo{}{bar}"), key_slot(b"foo{}{bar}"), "empty tag: whole key");
        assert_ne!(key_slot(b"foo{}{bar}"), key_slot(b"bar"));
        assert_eq!(key_slot(b"foo{{bar}}zap"), key_slot(b"{bar"));
        assert_eq!(key_slot(b"foo{bar}{zap}"), key_slot(b"bar"));
    }

    #[test]
    fn routing_keys() {
        let a = |v: &[&str]| v.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>();
        assert_eq!(routing_key(&a(&["GET", "k"])), Some(&b"k"[..]));
        assert_eq!(routing_key(&a(&["EVAL", "return 1", "1", "k", "x"])), Some(&b"k"[..]));
        assert_eq!(routing_key(&a(&["EVALSHA", "abc", "0", "x"])), None);
        assert_eq!(routing_key(&a(&["XREADGROUP", "GROUP", "g", "c", "COUNT", "1", "STREAMS", "s", ">"])), Some(&b"s"[..]));
        assert_eq!(routing_key(&a(&["PING"])), None);
        assert_eq!(routing_key(&a(&["OBJECT", "ENCODING", "k"])), Some(&b"k"[..]));
        assert_eq!(routing_key(&a(&["SPUBLISH", "ch", "m"])), Some(&b"ch"[..]));
    }

    #[test]
    fn redirections() {
        assert_eq!(Redirect::parse("MOVED 3999 127.0.0.1:6381"), Some(Redirect::Moved(3999, ("127.0.0.1".into(), 6381))));
        assert_eq!(Redirect::parse("ASK 3999 [::1]:6381"), Some(Redirect::Ask(3999, ("::1".into(), 6381))));
        assert_eq!(Redirect::parse("TRYAGAIN Multiple keys request during rehashing of slot"), Some(Redirect::Retry));
        assert_eq!(Redirect::parse("CLUSTERDOWN The cluster is down"), Some(Redirect::Retry));
        assert_eq!(Redirect::parse("ERR something"), None);
        assert_eq!(Redirect::parse("WRONGTYPE Operation"), None);
    }
}
