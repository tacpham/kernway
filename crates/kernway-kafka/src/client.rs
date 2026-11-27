//! The cluster client: metadata (brokers, partition leaders), a connection
//! per broker, topic administration, an idempotent producer, fetches,
//! offsets, and the consumer-group API at the group's coordinator.

use crate::conn::BrokerConn;
use crate::options::Options;
use crate::protocol::{Api, KafkaCode, R, W};
use crate::records::{decode_batches, encode_batch, ProducerIds, Record};
use kernway_messaging::MsgError;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

/// A partition's leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionMeta {
    /// Partition index.
    pub id: i32,
    /// Leader broker (-1: none right now).
    pub leader: i32,
    /// Leader epoch.
    pub leader_epoch: i32,
}

#[derive(Default)]
struct Meta {
    brokers: HashMap<i32, (String, u16)>,
    topics: HashMap<String, Vec<PartitionMeta>>,
}

#[derive(Default)]
struct Producing {
    /// (producer id, epoch) once initialized.
    ids: Option<(i64, i16)>,
    /// Next sequence per (topic, partition).
    seq: HashMap<(String, i32), i32>,
}

struct Inner {
    opts: Options,
    meta: RefCell<Meta>,
    conns: RefCell<HashMap<i32, BrokerConn>>,
    bootstrap: RefCell<Option<BrokerConn>>,
    producing: RefCell<Producing>,
    /// Round-robin counter for unkeyed messages.
    rr: RefCell<u64>,
}

/// A Kafka client. Cheap to clone; `!Send`.
#[derive(Clone)]
pub struct Client {
    inner: Rc<Inner>,
}

/// A fetched partition's records and marks.
#[derive(Debug, Clone, Default)]
pub struct Fetched {
    /// Records from the requested offset on.
    pub records: Vec<Record>,
    /// The partition's high watermark.
    pub high_watermark: i64,
    /// Error, if the partition answered with one.
    pub error: i16,
}

/// Kafka's murmur2 (the Java client's default partitioner), so keys land
/// on the same partitions as other clients put them.
pub fn murmur2(data: &[u8]) -> i32 {
    const SEED: u32 = 0x9747_b28c;
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;
    let len = data.len();
    let mut h: u32 = SEED ^ (len as u32);
    let (chunks, rest) = data.as_chunks::<4>();
    for c in chunks {
        let mut k = u32::from_le_bytes(*c);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        h = h.wrapping_mul(M);
        h ^= k;
    }
    match rest.len() {
        3 => {
            h ^= u32::from(rest[2]) << 16;
            h ^= u32::from(rest[1]) << 8;
            h ^= u32::from(rest[0]);
            h = h.wrapping_mul(M);
        }
        2 => {
            h ^= u32::from(rest[1]) << 8;
            h ^= u32::from(rest[0]);
            h = h.wrapping_mul(M);
        }
        1 => {
            h ^= u32::from(rest[0]);
            h = h.wrapping_mul(M);
        }
        _ => {}
    }
    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h as i32
}

/// The partition for a key among `n` (the Java client's formula).
pub fn partition_for(key: &[u8], n: usize) -> i32 {
    ((murmur2(key) & 0x7fff_ffff) as usize % n) as i32
}

fn code_err(code: i16, what: &str) -> Result<(), MsgError> {
    if code == 0 {
        Ok(())
    } else {
        Err(KafkaCode(code).err(what))
    }
}

impl Client {
    /// Connect to a bootstrap broker and load the cluster's metadata.
    pub async fn connect(opts: Options) -> Result<Self, MsgError> {
        let c = Client {
            inner: Rc::new(Inner {
                opts,
                meta: RefCell::default(),
                conns: RefCell::default(),
                bootstrap: RefCell::new(None),
                producing: RefCell::default(),
                rr: RefCell::new(0),
            }),
        };
        c.refresh(&[]).await?;
        Ok(c)
    }

    /// The options.
    pub fn options(&self) -> &Options {
        &self.inner.opts
    }

    async fn any_conn(&self) -> Result<BrokerConn, MsgError> {
        if let Some(c) = self.inner.bootstrap.borrow().as_ref().filter(|c| c.is_open()) {
            return Ok(c.clone());
        }
        // A known broker first, then the bootstrap list.
        let known: Vec<i32> = self.inner.meta.borrow().brokers.keys().copied().collect();
        for id in known {
            if let Ok(c) = self.conn(id).await {
                *self.inner.bootstrap.borrow_mut() = Some(c.clone());
                return Ok(c);
            }
        }
        let mut errors = Vec::new();
        for (h, p) in &self.inner.opts.bootstrap {
            match BrokerConn::connect(&self.inner.opts, h, *p).await {
                Ok(c) => {
                    *self.inner.bootstrap.borrow_mut() = Some(c.clone());
                    return Ok(c);
                }
                Err(e @ MsgError::Auth(_)) => return Err(e),
                Err(e) => errors.push(e.to_string()),
            }
        }
        Err(MsgError::Connection(format!("no Kafka broker reachable: {}", errors.join("; "))))
    }

    /// The connection to broker `id` (opened on first use).
    pub async fn conn(&self, id: i32) -> Result<BrokerConn, MsgError> {
        if let Some(c) = self.inner.conns.borrow().get(&id).filter(|c| c.is_open()) {
            return Ok(c.clone());
        }
        let addr = self.inner.meta.borrow().brokers.get(&id).cloned();
        let (h, p) = addr.ok_or_else(|| MsgError::Broker(format!("unknown Kafka broker {id}")))?;
        let c = BrokerConn::connect(&self.inner.opts, &h, p).await?;
        self.inner.conns.borrow_mut().insert(id, c.clone());
        Ok(c)
    }

    /// A new connection to broker `id`, not shared: for requests that wait
    /// long on the broker (group joins, fetch long-polls) — a broker answers
    /// one request per connection at a time, so they would hold up others.
    pub async fn dedicated(&self, id: i32) -> Result<BrokerConn, MsgError> {
        let addr = self.inner.meta.borrow().brokers.get(&id).cloned();
        let (h, p) = addr.ok_or_else(|| MsgError::Broker(format!("unknown Kafka broker {id}")))?;
        BrokerConn::connect(&self.inner.opts, &h, p).await
    }

    /// Reload metadata (`topics` empty: every topic).
    pub async fn refresh(&self, topics: &[&str]) -> Result<(), MsgError> {
        let c = self.any_conn().await?;
        let mut w = W::new(true);
        if topics.is_empty() {
            w.uvarint(0); // null: all topics
        } else {
            w.array_len(topics.len());
            for t in topics {
                w.uuid(&[0; 16]).nstring(Some(t)).tags();
            }
        }
        w.bool(false).bool(false).tags();
        let b = match c.call(Api::Metadata, &w.b).await {
            Ok(b) => b,
            Err(e) => {
                *self.inner.bootstrap.borrow_mut() = None;
                return Err(e);
            }
        };
        let mut r = R::new(&b, true);
        r.i32()?;
        let mut meta = self.inner.meta.borrow_mut();
        for _ in 0..r.array_len()? {
            let id = r.i32()?;
            let host = r.string()?;
            let port = r.i32()?;
            r.nstring()?;
            r.tags()?;
            meta.brokers.insert(id, (host, port as u16));
        }
        r.nstring()?; // cluster id
        r.i32()?; // controller
        for _ in 0..r.array_len()? {
            let code = r.i16()?;
            let name = r.nstring()?.unwrap_or_default();
            r.uuid()?;
            r.bool()?;
            let mut parts = Vec::new();
            for _ in 0..r.array_len()? {
                r.i16()?;
                let id = r.i32()?;
                let leader = r.i32()?;
                let leader_epoch = r.i32()?;
                for _ in 0..3 {
                    for _ in 0..r.array_len()? {
                        r.i32()?;
                    }
                }
                r.tags()?;
                parts.push(PartitionMeta { id, leader, leader_epoch });
            }
            r.i32()?;
            r.tags()?;
            parts.sort_by_key(|p| p.id);
            if code == 0 {
                meta.topics.insert(name, parts);
            } else {
                meta.topics.remove(&name);
            }
        }
        Ok(())
    }

    /// A topic's partitions (refreshing once when unknown or leaderless).
    pub async fn partitions(&self, topic: &str) -> Result<Vec<PartitionMeta>, MsgError> {
        for attempt in 0..20 {
            if let Some(p) = self.inner.meta.borrow().topics.get(topic).filter(|p| !p.is_empty() && p.iter().all(|x| x.leader >= 0)) {
                return Ok(p.clone());
            }
            if attempt > 0 {
                rt_core::sleep(Duration::from_millis(100)).await;
            }
            self.refresh(&[topic]).await?;
        }
        Err(KafkaCode(KafkaCode::UNKNOWN_TOPIC_OR_PARTITION).err(&format!("topic {topic}")))
    }

    async fn leader(&self, topic: &str, partition: i32) -> Result<BrokerConn, MsgError> {
        let parts = self.partitions(topic).await?;
        let p = parts.iter().find(|p| p.id == partition).ok_or_else(|| MsgError::Broker(format!("{topic} has no partition {partition}")))?;
        self.conn(p.leader).await
    }

    /// Forget metadata for a topic (after NOT_LEADER etc.).
    fn invalidate(&self, topic: &str) {
        self.inner.meta.borrow_mut().topics.remove(topic);
    }

    /// Create a topic (`Ok(false)`: it already existed).
    pub async fn create_topic(&self, topic: &str, partitions: i32, replication: i16) -> Result<bool, MsgError> {
        let c = self.any_conn().await?;
        let mut w = W::new(true);
        w.array_len(1).string(topic).i32(partitions).i16(replication).array_len(0).array_len(0).tags();
        w.i32(self.inner.opts.request_timeout.as_millis() as i32).bool(false).tags();
        let b = c.call(Api::CreateTopics, &w.b).await?;
        let mut r = R::new(&b, true);
        r.i32()?;
        r.array_len()?;
        r.string()?;
        let code = r.i16()?;
        let msg = r.nstring()?;
        match code {
            0 => {
                // Wait until every partition has a leader.
                self.partitions(topic).await?;
                Ok(true)
            }
            KafkaCode::TOPIC_ALREADY_EXISTS => Ok(false),
            _ => Err(MsgError::Broker(format!("create topic {topic}: {} {}", KafkaCode(code).name(), msg.unwrap_or_default()))),
        }
    }

    /// Delete a topic (`Ok(false)`: there was none).
    pub async fn delete_topic(&self, topic: &str) -> Result<bool, MsgError> {
        let c = self.any_conn().await?;
        let mut w = W::new(true);
        w.array_len(1).string(topic).i32(self.inner.opts.request_timeout.as_millis() as i32).tags();
        let b = c.call(Api::DeleteTopics, &w.b).await?;
        let mut r = R::new(&b, true);
        r.i32()?;
        r.array_len()?;
        r.string()?;
        let code = r.i16()?;
        self.invalidate(topic);
        match code {
            0 => Ok(true),
            KafkaCode::UNKNOWN_TOPIC_OR_PARTITION => Ok(false),
            _ => Err(KafkaCode(code).err(&format!("delete topic {topic}"))),
        }
    }

    /// The idempotent producer's id and epoch (initialized on first use).
    pub async fn producer_ids(&self) -> Result<(i64, i16), MsgError> {
        if let Some(ids) = self.inner.producing.borrow().ids {
            return Ok(ids);
        }
        let mut w = W::new(true);
        w.nstring(None).i32(60_000).tags();
        let mut attempt = 0;
        let ids = loop {
            let c = self.any_conn().await?;
            let b = c.call(Api::InitProducerId, &w.b).await?;
            let mut r = R::new(&b, true);
            r.i32()?;
            let code = r.i16()?;
            // The coordinator loads its state on first use: retriable.
            if KafkaCode(code).retriable() && attempt < 30 {
                attempt += 1;
                rt_core::sleep(Duration::from_millis(200)).await;
                continue;
            }
            code_err(code, "InitProducerId")?;
            break (r.i64()?, r.i16()?);
        };
        let mut p = self.inner.producing.borrow_mut();
        p.ids = Some(ids);
        p.seq.clear();
        Ok(ids)
    }

    /// Produce one batch exactly as given — the producer ids and sequence
    /// too — once, without retries: `(error code, base offset)`. For tests
    /// and tools; [`produce`](Self::produce) is the normal path.
    ///
    /// Do not mix the two on one client with its own
    /// [`producer_ids`](Self::producer_ids): `produce` keeps its own sequence numbers, and a
    /// sequence already used here makes the broker take the next `produce`
    /// for a duplicate and drop it.
    pub async fn produce_raw(&self, topic: &str, partition: i32, records: &[Record], ids: ProducerIds) -> Result<(i16, i64), MsgError> {
        let conn = self.leader(topic, partition).await?;
        let batch = encode_batch(records, ids);
        let mut w = W::new(true);
        w.nstring(None).i16(-1).i32(self.inner.opts.request_timeout.as_millis() as i32);
        w.array_len(1).string(topic).array_len(1).i32(partition).nbytes(Some(&batch)).tags().tags().tags();
        let b = conn.call(Api::Produce, &w.b).await?;
        let mut r = R::new(&b, true);
        r.array_len()?;
        r.string()?;
        r.array_len()?;
        r.i32()?;
        Ok((r.i16()?, r.i64()?))
    }

    /// Choose a partition: by key (murmur2), else round-robin.
    pub async fn choose_partition(&self, topic: &str, key: Option<&[u8]>) -> Result<i32, MsgError> {
        let n = self.partitions(topic).await?.len();
        Ok(match key {
            Some(k) => partition_for(k, n),
            None => {
                let mut rr = self.inner.rr.borrow_mut();
                *rr += 1;
                (*rr % n as u64) as i32
            }
        })
    }

    /// Produce one record (acks=all), retrying retriable errors; returns its offset.
    pub async fn produce(&self, topic: &str, partition: i32, record: Record) -> Result<i64, MsgError> {
        let idempotent = self.inner.opts.idempotent;
        let mut last = None;
        // The sequence is fixed for every retry of this record: the broker
        // drops a duplicate of an already written one.
        let mut ids = ProducerIds::NONE;
        let mut assigned = false;
        for attempt in 0..8 {
            if attempt > 0 {
                rt_core::sleep(Duration::from_millis(100 * attempt)).await;
            }
            let conn = match self.leader(topic, partition).await {
                Ok(c) => c,
                Err(e) => {
                    last = Some(e);
                    self.invalidate(topic);
                    continue;
                }
            };
            if idempotent && !assigned {
                let (id, epoch) = self.producer_ids().await?;
                // From here to the request being queued nothing awaits: the
                // sequence numbers go out in the order they are taken.
                let mut p = self.inner.producing.borrow_mut();
                let seq = p.seq.entry((topic.to_string(), partition)).or_insert(0);
                ids = ProducerIds { id, epoch, base_sequence: *seq };
                *seq = seq.wrapping_add(1) & i32::MAX;
                assigned = true;
            }
            let batch = encode_batch(std::slice::from_ref(&record), ids);
            let mut w = W::new(true);
            w.nstring(None).i16(-1).i32(self.inner.opts.request_timeout.as_millis() as i32);
            w.array_len(1).string(topic).array_len(1).i32(partition).nbytes(Some(&batch)).tags().tags().tags();
            let b = match conn.call(Api::Produce, &w.b).await {
                Ok(b) => b,
                Err(e) => {
                    // Unknown outcome: retried with the same sequence (no duplicate).
                    last = Some(e);
                    self.invalidate(topic);
                    continue;
                }
            };
            let mut r = R::new(&b, true);
            r.array_len()?;
            r.string()?;
            r.array_len()?;
            r.i32()?;
            let code = r.i16()?;
            let offset = r.i64()?;
            match code {
                0 | KafkaCode::DUPLICATE_SEQUENCE_NUMBER => return Ok(offset),
                KafkaCode::OUT_OF_ORDER_SEQUENCE_NUMBER | KafkaCode::UNKNOWN_PRODUCER_ID => {
                    // The producer's state was lost (or a gap): start a new identity.
                    self.inner.producing.borrow_mut().ids = None;
                    assigned = false;
                    last = Some(KafkaCode(code).err("produce"));
                }
                c if KafkaCode(c).retriable() => {
                    self.invalidate(topic);
                    last = Some(KafkaCode(c).err("produce"));
                }
                c => return Err(KafkaCode(c).err(&format!("produce to {topic}/{partition}"))),
            }
        }
        Err(last.unwrap_or_else(|| MsgError::Broker("produce failed".into())))
    }

    /// Fetch from one partition at `offset`, waiting up to `max_wait` for data.
    pub async fn fetch(&self, topic: &str, partition: i32, offset: i64, max_wait: Duration, max_bytes: i32) -> Result<Fetched, MsgError> {
        let mut m = self.fetch_many(topic, &[(partition, offset)], max_wait, max_bytes).await?;
        Ok(m.remove(&partition).unwrap_or_default())
    }

    /// Fetch several partitions of a topic (one request per leader), each
    /// from its offset; waits up to `max_wait` when nothing is there.
    pub async fn fetch_many(&self, topic: &str, at: &[(i32, i64)], max_wait: Duration, max_bytes: i32) -> Result<HashMap<i32, Fetched>, MsgError> {
        self.fetch_many_on(topic, at, max_wait, max_bytes, None).await
    }

    /// [`fetch_many`](Self::fetch_many) over the caller's own connections
    /// (`pool`, by broker id, opened as needed) — long-polls do not hold up
    /// the shared connections.
    pub async fn fetch_many_on(&self, topic: &str, at: &[(i32, i64)], max_wait: Duration, max_bytes: i32, pool: Option<&RefCell<HashMap<i32, BrokerConn>>>) -> Result<HashMap<i32, Fetched>, MsgError> {
        let parts = self.partitions(topic).await?;
        let mut by_leader: HashMap<i32, Vec<(i32, i64)>> = HashMap::new();
        for &(p, off) in at {
            let leader = parts.iter().find(|x| x.id == p).map_or(-1, |x| x.leader);
            by_leader.entry(leader).or_default().push((p, off));
        }
        let mut out = HashMap::new();
        for (leader, list) in by_leader {
            let conn = match pool {
                None => self.conn(leader).await?,
                Some(pool) => {
                    let have = pool.borrow().get(&leader).filter(|c| c.is_open()).cloned();
                    match have {
                        Some(c) => c,
                        None => {
                            let c = self.dedicated(leader).await?;
                            pool.borrow_mut().insert(leader, c.clone());
                            c
                        }
                    }
                }
            };
            let mut w = W::new(true);
            w.i32(-1).i32(max_wait.as_millis() as i32).i32(1).i32(max_bytes).i8(0).i32(0).i32(-1);
            w.array_len(1).string(topic).array_len(list.len());
            for &(p, off) in &list {
                w.i32(p).i32(-1).i64(off).i32(-1).i64(-1).i32(max_bytes).tags();
            }
            w.tags();
            w.array_len(0).string("").tags();
            let b = conn.call_timeout(Api::Fetch, &w.b, max_wait + self.inner.opts.request_timeout).await?;
            let mut r = R::new(&b, true);
            r.i32()?;
            code_err(r.i16()?, "fetch")?;
            r.i32()?;
            for _ in 0..r.array_len()? {
                r.string()?;
                for _ in 0..r.array_len()? {
                    let p = r.i32()?;
                    let mut f = Fetched { error: r.i16()?, high_watermark: r.i64()?, ..Fetched::default() };
                    r.i64()?;
                    r.i64()?;
                    for _ in 0..r.array_len()? {
                        r.i64()?;
                        r.i64()?;
                        r.tags()?;
                    }
                    r.i32()?;
                    let records = r.nbytes()?.unwrap_or_default();
                    r.tags()?;
                    if f.error == 0 {
                        let from = list.iter().find(|x| x.0 == p).map_or(0, |x| x.1);
                        f.records = decode_batches(&records, from, 64 << 20)?;
                    } else if KafkaCode(f.error).retriable() {
                        self.invalidate(topic);
                    }
                    out.insert(p, f);
                }
                r.tags()?;
            }
        }
        Ok(out)
    }

    /// The offset at `time` (-1 latest, -2 earliest) for each partition;
    /// retried while leadership settles (a new topic, a moved leader).
    pub async fn list_offsets(&self, topic: &str, time: i64) -> Result<HashMap<i32, i64>, MsgError> {
        let mut attempt = 0;
        loop {
            match self.list_offsets_once(topic, time).await {
                Err(MsgError::Broker(m)) if attempt < 30 && is_retriable_msg(&m) => {
                    attempt += 1;
                    self.invalidate(topic);
                    rt_core::sleep(Duration::from_millis(100)).await;
                }
                r => return r,
            }
        }
    }

    async fn list_offsets_once(&self, topic: &str, time: i64) -> Result<HashMap<i32, i64>, MsgError> {
        let parts = self.partitions(topic).await?;
        let mut by_leader: HashMap<i32, Vec<i32>> = HashMap::new();
        for p in &parts {
            by_leader.entry(p.leader).or_default().push(p.id);
        }
        let mut out = HashMap::new();
        for (leader, ids) in by_leader {
            let conn = self.conn(leader).await?;
            let mut w = W::new(true);
            w.i32(-1).i8(0).array_len(1).string(topic).array_len(ids.len());
            for id in &ids {
                w.i32(*id).i32(-1).i64(time).tags();
            }
            w.tags().tags();
            let b = conn.call(Api::ListOffsets, &w.b).await?;
            let mut r = R::new(&b, true);
            r.i32()?;
            for _ in 0..r.array_len()? {
                r.string()?;
                for _ in 0..r.array_len()? {
                    let p = r.i32()?;
                    let code = r.i16()?;
                    r.i64()?;
                    let off = r.i64()?;
                    r.i32()?;
                    r.tags()?;
                    if code != 0 {
                        self.invalidate(topic);
                        return Err(KafkaCode(code).err(&format!("list offsets {topic}/{p}")));
                    }
                    out.insert(p, off);
                }
                r.tags()?;
            }
        }
        Ok(out)
    }

    /// The group's coordinator connection (shared: short requests).
    pub async fn coordinator(&self, group: &str) -> Result<BrokerConn, MsgError> {
        let node = self.coordinator_node(group).await?;
        self.conn(node).await
    }

    /// The group's coordinator broker id.
    pub async fn coordinator_node(&self, group: &str) -> Result<i32, MsgError> {
        let mut last = None;
        for attempt in 0..20 {
            if attempt > 0 {
                rt_core::sleep(Duration::from_millis(200)).await;
            }
            let c = self.any_conn().await?;
            let mut w = W::new(true);
            w.i8(0).array_len(1).string(group).tags();
            let b = c.call(Api::FindCoordinator, &w.b).await?;
            let mut r = R::new(&b, true);
            r.i32()?;
            r.array_len()?;
            r.string()?;
            let node = r.i32()?;
            let host = r.string()?;
            let port = r.i32()?;
            let code = r.i16()?;
            if code == 0 {
                self.inner.meta.borrow_mut().brokers.entry(node).or_insert((host, port as u16));
                return Ok(node);
            }
            last = Some(KafkaCode(code).err("find coordinator"));
            if !KafkaCode(code).retriable() {
                break;
            }
        }
        Err(last.unwrap_or_else(|| MsgError::Broker("no coordinator".into())))
    }
}

/// Whether an error message names a retriable Kafka code.
pub fn is_retriable_msg(m: &str) -> bool {
    [
        "NOT_LEADER_OR_FOLLOWER",
        "LEADER_NOT_AVAILABLE",
        "UNKNOWN_TOPIC_OR_PARTITION",
        "COORDINATOR_LOAD_IN_PROGRESS",
        "COORDINATOR_NOT_AVAILABLE",
        "NOT_COORDINATOR",
        "REQUEST_TIMED_OUT",
        "FENCED_LEADER_EPOCH",
        "UNKNOWN_LEADER_EPOCH",
        "OFFSET_NOT_AVAILABLE",
        "NOT_ENOUGH_REPLICAS",
    ]
    .iter()
    .any(|c| m.contains(c))
}

/// The consumer protocol's subscription (v0): the topics.
pub fn encode_subscription(topics: &[&str]) -> Vec<u8> {
    let mut w = W::new(false);
    w.i16(0).array_len(topics.len());
    for t in topics {
        w.string(t);
    }
    w.nbytes(None);
    w.b
}

/// The consumer protocol's subscription topics.
pub fn decode_subscription(b: &[u8]) -> Result<Vec<String>, MsgError> {
    let mut r = R::new(b, false);
    r.i16()?;
    (0..r.array_len()?).map(|_| r.string()).collect()
}

/// The consumer protocol's assignment (v0): topic → partitions.
pub fn encode_assignment(a: &[(String, Vec<i32>)]) -> Vec<u8> {
    let mut w = W::new(false);
    w.i16(0).array_len(a.len());
    for (t, ps) in a {
        w.string(t).array_len(ps.len());
        for p in ps {
            w.i32(*p);
        }
    }
    w.nbytes(None);
    w.b
}

/// Decode an assignment (an empty one is no partitions).
pub fn decode_assignment(b: &[u8]) -> Result<Vec<(String, Vec<i32>)>, MsgError> {
    if b.is_empty() {
        return Ok(Vec::new());
    }
    let mut r = R::new(b, false);
    r.i16()?;
    let mut out = Vec::new();
    for _ in 0..r.array_len()? {
        let t = r.string()?;
        let ps = (0..r.array_len()?).map(|_| r.i32()).collect::<Result<_, _>>()?;
        out.push((t, ps));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn murmur2_matches_the_java_client() {
        // From Kafka's UtilsTest.testMurmur2.
        for (s, want) in [
            ("21", -973_932_308),
            ("foobar", -790_332_482),
            ("a-little-bit-long-string", -985_981_536),
            ("a-little-bit-longer-string", -1_486_304_829),
            ("lkjh234lh9fiuh90y23oiuhsafujhadof229phr9h19h89h8", -58_897_971),
            ("abc", 479_470_107),
        ] {
            assert_eq!(murmur2(s.as_bytes()), want, "{s}");
        }
        assert!((0..3).contains(&partition_for(b"k", 3)));
    }

    #[test]
    fn consumer_protocol_round_trips() {
        assert_eq!(decode_subscription(&encode_subscription(&["a", "b"])).unwrap(), ["a", "b"]);
        let a = vec![("t".to_string(), vec![0, 2]), ("u".to_string(), vec![])];
        assert_eq!(decode_assignment(&encode_assignment(&a)).unwrap(), a);
        assert!(decode_assignment(&[]).unwrap().is_empty());
    }
}
