//! The [`kernway_messaging::Broker`] backend on Kafka.
//!
//! Mapping of the contract: a **topic** is a Kafka topic (partitions from
//! the options); a **group** is the consumer group `<topic>.<group>`, its
//! members assigned partitions by the group leader (round-robin, client
//! side) — so a key, which always lands on one partition, is held by one
//! member at a time. A delivery is acknowledged by committing the group's
//! offset up to the lowest offset still unacknowledged in its partition,
//! synchronously; nack / ack_wait / dead letters are kept by the client
//! (Kafka has no per-message acknowledgement). The committed offset's
//! metadata records how far a partition was delivered, so the member that
//! takes it over flags those messages as redelivered.
//!
//! A new group's start position is committed when the group is created, so
//! "latest" means after the last message at that moment, and a group keeps
//! its position while no member is connected.

use crate::conn::BrokerConn;
use crate::client::{decode_assignment, decode_subscription, encode_assignment, encode_subscription, Client};
use crate::options::Options;
use crate::protocol::{Api, KafkaCode, R, W};
use crate::records::Record;
use kernway_messaging::{Broker, Capabilities, Consumer, Delivery, MsgError, MsgResult, OutMessage, Producer, Receipt, StartAt, Subscription};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// The key of the committed-offset metadata: how far the partition was delivered.
const DELIVERED: &str = "kw-delivered=";

/// The Kafka broker. Clones share one client.
#[derive(Clone)]
pub struct KafkaBroker {
    client: Client,
}

impl KafkaBroker {
    /// Connect with options.
    pub async fn connect(opts: Options) -> MsgResult<Self> {
        Ok(KafkaBroker { client: Client::connect(opts).await? })
    }

    /// Connect with a URL.
    pub async fn connect_url(url: &str) -> MsgResult<Self> {
        Self::connect(Options::parse(url)?).await
    }

    /// The client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The Kafka group id of a contract group.
    pub fn group_id(topic: &str, group: &str) -> String {
        format!("{topic}.{group}")
    }
}

/// Publishes (idempotent, acks=all).
pub struct KafkaProducer {
    client: Client,
}

impl Producer for KafkaProducer {
    async fn publish(&self, topic: &str, msg: OutMessage) -> MsgResult<Receipt> {
        let p = self.client.choose_partition(topic, msg.key.as_deref()).await?;
        let rec = Record {
            offset: 0,
            timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64),
            key: msg.key,
            value: Some(msg.payload),
            headers: msg.headers.into_iter().map(|(k, v)| (k, Some(v.into_bytes()))).collect(),
        };
        let off = self.client.produce(topic, p, rec).await?;
        Ok(Receipt { id: format!("{p}:{off}") })
    }
}

// ---- group membership ----------------------------------------------------------

#[derive(Default)]
struct Member {
    member_id: String,
    generation: i32,
    partitions: Vec<i32>,
    /// Set when the background task must join again (rebalance).
    rejoin: bool,
    error: Option<MsgError>,
    closed: bool,
}

struct GroupCtx {
    client: Client,
    group: String,
    topic: String,
    session: Duration,
    m: RefCell<Member>,
    /// Join / sync / heartbeat / leave go on a connection of their own: a
    /// join waits for the whole group, and would hold up anything behind it.
    coord: RefCell<Option<BrokerConn>>,
}

impl GroupCtx {
    async fn coord(&self) -> MsgResult<BrokerConn> {
        if let Some(c) = self.coord.borrow().as_ref().filter(|c| c.is_open()) {
            return Ok(c.clone());
        }
        let node = self.client.coordinator_node(&self.group).await?;
        let c = self.client.dedicated(node).await?;
        *self.coord.borrow_mut() = Some(c.clone());
        Ok(c)
    }

    async fn join(&self) -> MsgResult<()> {
        let coord = self.coord().await?;
        let rebalance = self.session * 3;
        loop {
            let member_id = self.m.borrow().member_id.clone();
            let mut w = W::new(true);
            w.string(&self.group).i32(self.session.as_millis() as i32).i32(rebalance.as_millis() as i32).string(&member_id).nstring(None).string("consumer");
            w.array_len(1).string("roundrobin").bytes(&encode_subscription(&[&self.topic])).tags().tags();
            let b = coord.call_timeout(Api::JoinGroup, &w.b, rebalance + Duration::from_secs(10)).await?;
            let mut r = R::new(&b, true);
            r.i32()?;
            let code = r.i16()?;
            let generation = r.i32()?;
            r.nstring()?;
            r.nstring()?;
            let leader = r.string()?;
            let me = r.string()?;
            let mut members = Vec::new();
            for _ in 0..r.array_len()? {
                let id = r.string()?;
                r.nstring()?;
                let meta = r.bytes()?;
                r.tags()?;
                members.push((id, meta));
            }
            match code {
                0 => {}
                KafkaCode::MEMBER_ID_REQUIRED => {
                    self.m.borrow_mut().member_id = me;
                    continue;
                }
                KafkaCode::UNKNOWN_MEMBER_ID => {
                    self.m.borrow_mut().member_id.clear();
                    continue;
                }
                c if KafkaCode(c).retriable() || c == KafkaCode::REBALANCE_IN_PROGRESS => {
                    if matches!(c, KafkaCode::NOT_COORDINATOR | KafkaCode::COORDINATOR_NOT_AVAILABLE) {
                        *self.coord.borrow_mut() = None;
                        return Err(KafkaCode(c).err("join group"));
                    }
                    rt_core::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                c => return Err(KafkaCode(c).err(&format!("join group {}", self.group))),
            }
            // The leader assigns: partitions round-robin over the members, in id order.
            let mut assignments = Vec::new();
            if leader == me {
                self.client.refresh(&[&self.topic]).await?;
                let parts = self.client.partitions(&self.topic).await?;
                let mut ids: Vec<&String> = members.iter().filter(|(_, meta)| decode_subscription(meta).is_ok_and(|t| t.contains(&self.topic))).map(|(id, _)| id).collect();
                ids.sort();
                let mut per: BTreeMap<&String, Vec<i32>> = ids.iter().map(|id| (*id, Vec::new())).collect();
                for (i, p) in parts.iter().enumerate() {
                    if let Some(id) = ids.get(i % ids.len().max(1)) {
                        if let Some(v) = per.get_mut(id) {
                            v.push(p.id);
                        }
                    }
                }
                for (id, ps) in per {
                    assignments.push((id.clone(), encode_assignment(&[(self.topic.clone(), ps)])));
                }
            }
            let mut w = W::new(true);
            w.string(&self.group).i32(generation).string(&me).nstring(None).nstring(Some("consumer")).nstring(Some("roundrobin"));
            w.array_len(assignments.len());
            for (id, a) in &assignments {
                w.string(id).bytes(a).tags();
            }
            w.tags();
            let b = coord.call_timeout(Api::SyncGroup, &w.b, rebalance + Duration::from_secs(10)).await?;
            let mut r = R::new(&b, true);
            r.i32()?;
            let code = r.i16()?;
            r.nstring()?;
            r.nstring()?;
            let assignment = r.bytes()?;
            match code {
                0 => {}
                KafkaCode::REBALANCE_IN_PROGRESS | KafkaCode::ILLEGAL_GENERATION => continue,
                KafkaCode::UNKNOWN_MEMBER_ID => {
                    self.m.borrow_mut().member_id.clear();
                    continue;
                }
                c => return Err(KafkaCode(c).err(&format!("sync group {}", self.group))),
            }
            let mut partitions: Vec<i32> = decode_assignment(&assignment)?.into_iter().filter(|(t, _)| *t == self.topic).flat_map(|(_, ps)| ps).collect();
            partitions.sort_unstable();
            let mut m = self.m.borrow_mut();
            m.member_id = me;
            m.generation = generation;
            m.partitions = partitions;
            m.rejoin = false;
            return Ok(());
        }
    }

    async fn heartbeat(&self) -> MsgResult<i16> {
        let coord = self.coord().await?;
        let (generation, member_id) = {
            let m = self.m.borrow();
            (m.generation, m.member_id.clone())
        };
        let mut w = W::new(true);
        w.string(&self.group).i32(generation).string(&member_id).nstring(None).tags();
        let b = coord.call(Api::Heartbeat, &w.b).await?;
        let mut r = R::new(&b, true);
        r.i32()?;
        r.i16()
    }

    async fn leave(&self) {
        let member_id = self.m.borrow().member_id.clone();
        if member_id.is_empty() {
            return;
        }
        if let Ok(coord) = self.coord().await {
            let mut w = W::new(true);
            w.string(&self.group).array_len(1).string(&member_id).nstring(None).tags().tags();
            let _ = coord.call(Api::LeaveGroup, &w.b).await;
        }
    }

    /// Committed offsets and their metadata, per partition (-1: none);
    /// retried while the coordinator loads or moves.
    async fn committed(&self, partitions: &[i32]) -> MsgResult<HashMap<i32, (i64, Option<String>)>> {
        let mut attempt = 0;
        loop {
            match self.committed_once(partitions).await {
                Err(MsgError::Broker(m)) if attempt < 50 && crate::client::is_retriable_msg(&m) => {
                    attempt += 1;
                    rt_core::sleep(Duration::from_millis(100)).await;
                }
                r => return r,
            }
        }
    }

    async fn committed_once(&self, partitions: &[i32]) -> MsgResult<HashMap<i32, (i64, Option<String>)>> {
        let coord = self.client.coordinator(&self.group).await?;
        let mut w = W::new(true);
        w.string(&self.group).array_len(1).string(&self.topic).array_len(partitions.len());
        for p in partitions {
            w.i32(*p);
        }
        w.tags().bool(false).tags();
        let b = coord.call(Api::OffsetFetch, &w.b).await?;
        let mut r = R::new(&b, true);
        r.i32()?;
        let mut out = HashMap::new();
        for _ in 0..r.array_len()? {
            r.string()?;
            for _ in 0..r.array_len()? {
                let p = r.i32()?;
                let off = r.i64()?;
                r.i32()?;
                let meta = r.nstring()?;
                let code = r.i16()?;
                r.tags()?;
                if code != 0 {
                    return Err(KafkaCode(code).err("offset fetch"));
                }
                out.insert(p, (off, meta));
            }
            r.tags()?;
        }
        let code = r.i16()?;
        if code != 0 {
            return Err(KafkaCode(code).err("offset fetch"));
        }
        Ok(out)
    }

    /// Commit offsets (`generation` -1 and no member: a group without
    /// members); retried while the coordinator loads or moves.
    async fn commit(&self, generation: i32, member_id: &str, offsets: &[(i32, i64, String)]) -> MsgResult<()> {
        let mut attempt = 0;
        loop {
            match self.commit_once(generation, member_id, offsets).await {
                Err(MsgError::Broker(m)) if attempt < 50 && crate::client::is_retriable_msg(&m) => {
                    attempt += 1;
                    rt_core::sleep(Duration::from_millis(100)).await;
                }
                r => return r,
            }
        }
    }

    async fn commit_once(&self, generation: i32, member_id: &str, offsets: &[(i32, i64, String)]) -> MsgResult<()> {
        let coord = self.client.coordinator(&self.group).await?;
        let mut w = W::new(true);
        w.string(&self.group).i32(generation).string(member_id).nstring(None);
        w.array_len(1).string(&self.topic).array_len(offsets.len());
        for (p, off, meta) in offsets {
            w.i32(*p).i64(*off).i32(-1).nstring(Some(meta)).tags();
        }
        w.tags().tags();
        let b = coord.call(Api::OffsetCommit, &w.b).await?;
        let mut r = R::new(&b, true);
        r.i32()?;
        for _ in 0..r.array_len()? {
            r.string()?;
            for _ in 0..r.array_len()? {
                r.i32()?;
                let code = r.i16()?;
                r.tags()?;
                if code != 0 {
                    return Err(KafkaCode(code).err("offset commit"));
                }
            }
            r.tags()?;
        }
        Ok(())
    }
}

/// Keeps the membership alive: heartbeats, and joins again on a rebalance.
async fn membership(weak: Weak<GroupCtx>) {
    loop {
        let Some(ctx) = weak.upgrade() else { return };
        if ctx.m.borrow().closed {
            return;
        }
        let every = ctx.session / 3;
        if ctx.m.borrow().rejoin {
            if let Err(e) = ctx.join().await {
                if matches!(e, MsgError::Auth(_) | MsgError::Config(_)) {
                    ctx.m.borrow_mut().error = Some(e);
                    return;
                }
                drop(ctx);
                rt_core::sleep(Duration::from_millis(500)).await;
                continue;
            }
        } else {
            match ctx.heartbeat().await {
                Ok(0) => {}
                Ok(KafkaCode::UNKNOWN_MEMBER_ID) => {
                    let mut m = ctx.m.borrow_mut();
                    m.member_id.clear();
                    m.rejoin = true;
                    continue;
                }
                Ok(_) => {
                    ctx.m.borrow_mut().rejoin = true;
                    continue;
                }
                Err(_) => {}
            }
        }
        drop(ctx);
        // Short sleeps, so a rebalance someone else starts is answered soon.
        let until = Instant::now() + every;
        while Instant::now() < until {
            rt_core::sleep(Duration::from_millis(100)).await;
            match weak.upgrade() {
                Some(c) if c.m.borrow().closed => return,
                Some(c) if c.m.borrow().rejoin => break,
                Some(_) => {}
                None => return,
            }
        }
    }
}

// ---- the consumer --------------------------------------------------------------

#[derive(Default)]
struct Part {
    /// Next offset to fetch.
    pos: i64,
    /// Fetched, not yet handed out (in offset order).
    buffered: VecDeque<Record>,
    /// Handed out, not yet settled: offset → (record, since).
    held: BTreeMap<i64, (Record, Instant)>,
    /// Offsets at or below this were delivered before (redelivered).
    delivered_upto: i64,
    /// Offset committed.
    committed: i64,
}

impl Part {
    /// The offset to commit: the first one not yet settled.
    fn low_water(&self) -> i64 {
        let held = self.held.keys().next().copied();
        let buf = self.buffered.front().map(|r| r.offset);
        [held, buf].into_iter().flatten().min().unwrap_or(self.pos)
    }
}

/// A group member.
pub struct KafkaConsumer {
    client: Client,
    sub: Subscription,
    ctx: Rc<GroupCtx>,
    generation: i32,
    parts: BTreeMap<i32, Part>,
    rr: usize,
    /// Fetch connections of its own (long-polls), by broker.
    fetch_conns: RefCell<HashMap<i32, BrokerConn>>,
}

fn parse_meta(m: &Option<String>) -> i64 {
    m.as_deref().and_then(|s| s.strip_prefix(DELIVERED)).and_then(|s| s.parse().ok()).unwrap_or(-1)
}

impl KafkaConsumer {
    /// Take the current assignment (after a rebalance): positions from the
    /// committed offsets; what this member held is now stale.
    async fn sync_assignment(&mut self) -> MsgResult<()> {
        let (generation, partitions) = {
            let m = self.ctx.m.borrow();
            (m.generation, m.partitions.clone())
        };
        if generation == self.generation {
            return Ok(());
        }
        let committed = self.ctx.committed(&partitions).await?;
        let mut missing = false;
        let mut parts = BTreeMap::new();
        for p in &partitions {
            let (off, meta) = committed.get(p).cloned().unwrap_or((-1, None));
            missing |= off < 0;
            parts.insert(*p, Part { pos: off, delivered_upto: parse_meta(&meta), committed: off, ..Part::default() });
        }
        if missing {
            let time = if self.sub.start == StartAt::Earliest { -2 } else { -1 };
            let offs = self.client.list_offsets(&self.sub.topic, time).await?;
            for (p, part) in parts.iter_mut() {
                if part.pos < 0 {
                    part.pos = offs.get(p).copied().unwrap_or(0);
                    part.committed = -1;
                }
            }
        }
        self.parts = parts;
        self.generation = generation;
        Ok(())
    }

    fn held_keys(&self) -> HashSet<Vec<u8>> {
        self.parts.values().flat_map(|p| p.held.values().filter_map(|(r, _)| r.key.clone())).collect()
    }

    fn held_count(&self) -> usize {
        self.parts.values().map(|p| p.held.len()).sum()
    }

    /// Hand out the next record a key's order allows.
    fn take(&mut self) -> Option<Delivery> {
        if self.held_count() >= self.sub.max_in_flight {
            return None;
        }
        let keys = self.held_keys();
        let ids: Vec<i32> = self.parts.keys().copied().collect();
        for i in 0..ids.len() {
            let p = ids[(self.rr + i) % ids.len()];
            let part = self.parts.get_mut(&p)?;
            let Some(head) = part.buffered.front() else { continue };
            if head.key.as_ref().is_some_and(|k| keys.contains(k)) {
                continue; // an earlier message of this key is still in flight
            }
            let rec = part.buffered.pop_front()?;
            let redelivered = rec.offset <= part.delivered_upto;
            part.delivered_upto = part.delivered_upto.max(rec.offset);
            part.held.insert(rec.offset, (rec.clone(), Instant::now()));
            self.rr = (self.rr + i + 1) % ids.len().max(1);
            return Some(Delivery {
                topic: self.sub.topic.clone(),
                key: rec.key,
                payload: rec.value.unwrap_or_default(),
                headers: rec.headers.into_iter().map(|(k, v)| (k, String::from_utf8_lossy(&v.unwrap_or_default()).into_owned())).collect(),
                redelivered,
                tag: format!("{}:{p}:{}", self.generation, rec.offset),
            });
        }
        None
    }

    /// Held past the ack wait: back in front of the queue.
    fn expire(&mut self) {
        let wait = self.sub.ack_wait;
        for part in self.parts.values_mut() {
            let expired: Vec<i64> = part.held.iter().filter(|(_, (_, since))| since.elapsed() >= wait).map(|(o, _)| *o).collect();
            for o in expired.into_iter().rev() {
                if let Some((rec, _)) = part.held.remove(&o) {
                    part.buffered.push_front(rec);
                }
            }
            part.buffered.make_contiguous().sort_by_key(|r| r.offset);
        }
    }

    /// Fetch the partitions with nothing buffered. `false`: there were none
    /// (everything buffered waits on an in-flight key or the in-flight limit).
    async fn fill(&mut self, wait: Duration) -> MsgResult<bool> {
        let at: Vec<(i32, i64)> = self.parts.iter().filter(|(_, p)| p.buffered.is_empty()).map(|(id, p)| (*id, p.pos)).collect();
        if at.is_empty() {
            return Ok(false);
        }
        let fetched = self.client.fetch_many_on(&self.sub.topic, &at, wait, 4 << 20, Some(&self.fetch_conns)).await?;
        for (p, f) in fetched {
            let Some(part) = self.parts.get_mut(&p) else { continue };
            match f.error {
                0 => {
                    for r in f.records {
                        if r.offset >= part.pos {
                            part.pos = r.offset + 1;
                            part.buffered.push_back(r);
                        }
                    }
                }
                KafkaCode::OFFSET_OUT_OF_RANGE => {
                    let offs = self.client.list_offsets(&self.sub.topic, -2).await?;
                    part.pos = offs.get(&p).copied().unwrap_or(0);
                }
                _ => {}
            }
        }
        Ok(true)
    }

    fn parse_tag(&self, d: &Delivery) -> Option<(i32, i64)> {
        let mut it = d.tag.split(':');
        let (g, p, o) = (it.next()?.parse::<i32>().ok()?, it.next()?.parse::<i32>().ok()?, it.next()?.parse::<i64>().ok()?);
        (g == self.generation && self.parts.get(&p).is_some_and(|x| x.held.contains_key(&o))).then_some((p, o))
    }

    /// Commit partitions whose low water moved (or `force`: with the
    /// delivered mark, on close).
    async fn commit(&mut self, only: Option<i32>, force: bool) -> MsgResult<()> {
        let (generation, member_id) = {
            let m = self.ctx.m.borrow();
            (m.generation, m.member_id.clone())
        };
        if generation != self.generation {
            return Ok(()); // rebalanced: positions belong to the new owner
        }
        let mut offsets = Vec::new();
        for (p, part) in &self.parts {
            if only.is_some_and(|o| o != *p) {
                continue;
            }
            let lw = part.low_water();
            if lw > part.committed || (force && part.delivered_upto >= lw) {
                offsets.push((*p, lw, format!("{DELIVERED}{}", part.delivered_upto)));
            }
        }
        if offsets.is_empty() {
            return Ok(());
        }
        match self.ctx.commit(generation, &member_id, &offsets).await {
            Ok(()) => {
                for (p, lw, _) in offsets {
                    if let Some(part) = self.parts.get_mut(&p) {
                        part.committed = lw;
                    }
                }
                Ok(())
            }
            // A rebalance took the partition: the new owner redelivers.
            Err(MsgError::Broker(m)) if m.contains("REBALANCE") || m.contains("ILLEGAL_GENERATION") || m.contains("UNKNOWN_MEMBER") => {
                self.ctx.m.borrow_mut().rejoin = true;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Consumer for KafkaConsumer {
    async fn next(&mut self) -> MsgResult<Delivery> {
        loop {
            match self.step().await {
                Ok(Some(d)) => return Ok(d),
                Ok(None) => {}
                // Lost connections (a broker restart): heal and go on —
                // what was held but not acknowledged comes again.
                Err(MsgError::Connection(_) | MsgError::Timeout) => {
                    self.fetch_conns.borrow_mut().clear();
                    rt_core::sleep(Duration::from_millis(300)).await;
                }
                Err(MsgError::Broker(m)) if crate::client::is_retriable_msg(&m) => {
                    rt_core::sleep(Duration::from_millis(300)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn ack(&mut self, d: &Delivery) -> MsgResult<()> {
        self.ack_inner(d).await
    }

    async fn nack(&mut self, d: &Delivery, requeue: bool) -> MsgResult<()> {
        self.nack_inner(d, requeue).await
    }

    async fn close(mut self) -> MsgResult<()> {
        // Record how far it delivered, so the next owner flags redeliveries.
        let _ = self.commit(None, true).await;
        self.ctx.m.borrow_mut().closed = true;
        self.ctx.leave().await;
        Ok(())
    }
}

impl KafkaConsumer {
    /// One round: a delivery, or `None` after waiting for data.
    async fn step(&mut self) -> MsgResult<Option<Delivery>> {
        if let Some(e) = self.ctx.m.borrow().error.clone() {
            return Err(e);
        }
        self.sync_assignment().await?;
        self.expire();
        if let Some(d) = self.take() {
            return Ok(Some(d));
        }
        if self.parts.is_empty() {
            rt_core::sleep(Duration::from_millis(100)).await;
            return Ok(None);
        }
        // Wait for data — but not past the next ack-wait expiry.
        let wait = self
            .parts
            .values()
            .flat_map(|p| p.held.values().map(|(_, s)| self.sub.ack_wait.saturating_sub(s.elapsed())))
            .min()
            .unwrap_or(Duration::from_millis(500))
            .clamp(Duration::from_millis(10), Duration::from_millis(500));
        // Nothing to fetch means what is buffered waits for an ack or an
        // ack-wait expiry: sleep (cancellable) — never spin, which would
        // starve the timers and the group's heartbeats on this shard.
        if !self.fill(wait).await? {
            rt_core::sleep(wait).await;
        }
        Ok(None)
    }

    async fn ack_inner(&mut self, d: &Delivery) -> MsgResult<()> {
        let Some((p, o)) = self.parse_tag(d) else { return Ok(()) };
        if let Some(part) = self.parts.get_mut(&p) {
            part.held.remove(&o);
        }
        self.commit(Some(p), false).await
    }

    async fn nack_inner(&mut self, d: &Delivery, requeue: bool) -> MsgResult<()> {
        let Some((p, o)) = self.parse_tag(d) else { return Ok(()) };
        if requeue {
            if let Some(part) = self.parts.get_mut(&p) {
                if let Some((rec, _)) = part.held.remove(&o) {
                    part.buffered.push_front(rec);
                    part.buffered.make_contiguous().sort_by_key(|r| r.offset);
                }
            }
            return Ok(());
        }
        if let Some(dl) = self.sub.dead_letter.clone() {
            let rec = self.parts.get(&p).and_then(|x| x.held.get(&o)).map(|(r, _)| r.clone());
            if let Some(mut rec) = rec {
                rec.headers.push(("kw-dead-letter-from".into(), Some(self.sub.topic.clone().into_bytes())));
                let dp = self.client.choose_partition(&dl, rec.key.as_deref()).await?;
                self.client.produce(&dl, dp, rec).await?;
            }
        }
        self.ack_inner(d).await
    }
}

impl Drop for KafkaConsumer {
    fn drop(&mut self) {
        self.ctx.m.borrow_mut().closed = true;
    }
}

impl Broker for KafkaBroker {
    type Producer = KafkaProducer;
    type Consumer = KafkaConsumer;

    async fn producer(&self) -> MsgResult<KafkaProducer> {
        Ok(KafkaProducer { client: self.client.clone() })
    }

    async fn subscribe(&self, sub: Subscription) -> MsgResult<KafkaConsumer> {
        self.ensure_topic(&sub.topic).await?;
        let ctx = Rc::new(GroupCtx {
            client: self.client.clone(),
            group: Self::group_id(&sub.topic, &sub.group),
            topic: sub.topic.clone(),
            session: self.client.options().session_timeout,
            m: RefCell::default(),
            coord: RefCell::new(None),
        });
        // A new group's position is fixed now (latest: after the last message).
        let parts: Vec<i32> = self.client.partitions(&sub.topic).await?.iter().map(|p| p.id).collect();
        let committed = ctx.committed(&parts).await?;
        if parts.iter().all(|p| committed.get(p).is_none_or(|(o, _)| *o < 0)) {
            let time = if sub.start == StartAt::Earliest { -2 } else { -1 };
            let offs = self.client.list_offsets(&sub.topic, time).await?;
            let initial: Vec<(i32, i64, String)> = parts.iter().map(|p| (*p, offs.get(p).copied().unwrap_or(0), format!("{DELIVERED}-1"))).collect();
            // Only a group without members accepts this; a racing member's
            // own commit wins otherwise.
            let _ = ctx.commit(-1, "", &initial).await;
        }
        ctx.join().await?;
        rt_core::spawn(membership(Rc::downgrade(&ctx)));
        let mut c = KafkaConsumer { client: self.client.clone(), sub, ctx, generation: -1, parts: BTreeMap::new(), rr: 0, fetch_conns: RefCell::default() };
        c.sync_assignment().await?;
        Ok(c)
    }

    async fn ensure_topic(&self, topic: &str) -> MsgResult<()> {
        let o = self.client.options();
        self.client.create_topic(topic, o.partitions, o.replication).await.map(|_| ())
    }

    async fn delete_topic(&self, topic: &str) -> MsgResult<()> {
        self.client.delete_topic(topic).await.map(|_| ())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { replay: true, ack_timeout: true, dead_letter: true, key_order_across_members: true }
    }
}
