//! The [`kernway_messaging::Broker`] backend on RabbitMQ.
//!
//! Mapping of the contract: a **topic** is a durable `fanout` exchange of
//! that name; a **group** is a durable quorum queue `<topic>.<group>` bound
//! to it, its members consuming from the queue. Publishes are persistent,
//! mandatory and wait for the publisher confirm. Rejected deliveries go to
//! the subscription's dead-letter topic through RabbitMQ's own dead
//! lettering (`x-dead-letter-exchange`, at-least-once strategy). RabbitMQ
//! has no per-delivery ack timeout, so `ack_wait` is kept by the consumer:
//! a delivery held past it is requeued (and a late ack of it ignored).
//!
//! What RabbitMQ cannot do here: a group created after messages were
//! published does not see them (a queue only receives what arrives after it
//! is bound — [`Capabilities::replay`] is false), and two members of a group
//! may hold messages of one key at the same time.

use crate::codec::{Field, Properties, Table};
use crate::conn::{Channel, Connection};
use crate::options::Options;
use kernway_messaging::{Broker, Capabilities, Consumer, Delivery, MsgError, MsgResult, OutMessage, Producer, Receipt, Subscription};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// The header carrying a message's key.
pub const KEY_HEADER: &str = "kw-key";

struct Inner {
    opts: Options,
    conn: RefCell<Option<Connection>>,
    /// Groups (queues) created through this broker, per topic — removed by `delete_topic`.
    groups: RefCell<HashMap<String, HashSet<String>>>,
    connects: Cell<u64>,
}

/// The RabbitMQ broker. Clones share one connection (re-established when lost).
#[derive(Clone)]
pub struct RabbitBroker {
    inner: Rc<Inner>,
}

impl RabbitBroker {
    /// Connect with options.
    pub async fn connect(opts: Options) -> MsgResult<Self> {
        let b = RabbitBroker { inner: Rc::new(Inner { opts, conn: RefCell::new(None), groups: RefCell::default(), connects: Cell::new(0) }) };
        b.connection().await?;
        Ok(b)
    }

    /// Connect with a URI.
    pub async fn connect_uri(uri: &str) -> MsgResult<Self> {
        Self::connect(Options::parse(uri)?).await
    }

    /// The live connection, reconnecting when it was lost.
    pub async fn connection(&self) -> MsgResult<Connection> {
        if let Some(c) = self.inner.conn.borrow().as_ref().filter(|c| c.is_open()) {
            return Ok(c.clone());
        }
        let c = Connection::connect(self.inner.opts.clone()).await?;
        self.inner.connects.set(self.inner.connects.get() + 1);
        *self.inner.conn.borrow_mut() = Some(c.clone());
        Ok(c)
    }

    /// Whether the connection is up now (it is re-established on use).
    pub fn is_connected(&self) -> bool {
        self.inner.conn.borrow().as_ref().is_some_and(Connection::is_open)
    }

    /// How many times the broker (re)connected.
    pub fn connects(&self) -> u64 {
        self.inner.connects.get()
    }

    /// Run `f` on a fresh channel (a failed declare closes its channel, not ours).
    async fn with_channel<T, F, Fut>(&self, f: F) -> MsgResult<T>
    where
        F: FnOnce(Channel) -> Fut,
        Fut: std::future::Future<Output = (Channel, MsgResult<T>)>,
    {
        let ch = self.connection().await?.channel().await?;
        let (ch, r) = f(ch).await;
        let _ = ch.close().await;
        r
    }

    /// The queue for `group` on `topic`.
    pub fn queue_name(topic: &str, group: &str) -> String {
        format!("{topic}.{group}")
    }
}

/// Application headers: names that repeat become an array.
fn to_table(headers: &[(String, String)], key: Option<&[u8]>) -> Table {
    let mut t: Table = Vec::new();
    for (k, v) in headers {
        match t.iter_mut().find(|(n, _)| n == k) {
            Some((_, Field::Array(a))) => a.push(Field::str(v)),
            Some((_, f)) => {
                let first = std::mem::replace(f, Field::Void);
                *f = Field::Array(vec![first, Field::str(v)]);
            }
            None => t.push((k.clone(), Field::str(v))),
        }
    }
    if let Some(k) = key {
        t.push((KEY_HEADER.into(), Field::Bytes(k.to_vec())));
    }
    t
}

/// Back to `(name, text)` pairs plus the key. Nested tables (RabbitMQ's
/// `x-death`) are left out.
fn from_table(t: Option<Table>) -> (Vec<(String, String)>, Option<Vec<u8>>, i64) {
    let mut headers = Vec::new();
    let mut key = None;
    let mut delivery_count = 0;
    for (k, v) in t.unwrap_or_default() {
        if k == KEY_HEADER {
            if let Field::Bytes(b) | Field::Str(b) = v {
                key = Some(b);
            }
            continue;
        }
        if k == "x-delivery-count" {
            delivery_count = v.as_i64().unwrap_or(0);
        }
        match v {
            Field::Array(a) => headers.extend(a.iter().filter_map(Field::as_text).map(|s| (k.clone(), s))),
            other => {
                if let Some(s) = other.as_text() {
                    headers.push((k, s));
                }
            }
        }
    }
    (headers, key, delivery_count)
}

/// Publishes with confirms.
pub struct RabbitProducer {
    broker: RabbitBroker,
    chan: RefCell<Option<Rc<Channel>>>,
    seq: Cell<u64>,
}

impl RabbitProducer {
    async fn channel(&self) -> MsgResult<Rc<Channel>> {
        if let Some(c) = self.chan.borrow().as_ref().filter(|c| c.is_open()) {
            return Ok(c.clone());
        }
        let c = self.broker.connection().await?.channel().await?;
        c.confirm_select().await?;
        let c = Rc::new(c);
        *self.chan.borrow_mut() = Some(c.clone());
        Ok(c)
    }
}

impl Producer for RabbitProducer {
    async fn publish(&self, topic: &str, msg: OutMessage) -> MsgResult<Receipt> {
        let ch = self.channel().await?;
        self.seq.set(self.seq.get() + 1);
        let id = format!("kw-{}-{}", std::process::id(), self.seq.get());
        let props = Properties {
            headers: Some(to_table(&msg.headers, msg.key.as_deref())),
            delivery_mode: Some(2),
            message_id: Some(id.clone()),
            ..Properties::default()
        };
        let seq = ch.publish(topic, "", true, &props, &msg.payload)?;
        let r = ch.wait_confirm(seq).await;
        if r.is_err() {
            // The channel is gone (404: no such topic …): next publish opens another.
            *self.chan.borrow_mut() = None;
        }
        if !r? {
            return Err(MsgError::Broker(format!("the broker refused the message for {topic} (nack)")));
        }
        // A return comes before the confirm of its message.
        let mut unrouted = false;
        while let Some(ret) = ch.take_return() {
            if ret.props.message_id.as_deref() == Some(id.as_str()) {
                unrouted = true;
            }
        }
        Ok(Receipt { id: if unrouted { "unrouted: the topic has no group".into() } else { seq.to_string() } })
    }
}

struct Held {
    tag: u64,
    since: Instant,
}

/// A group member.
pub struct RabbitConsumer {
    broker: RabbitBroker,
    sub: Subscription,
    queue: String,
    chan: Option<Channel>,
    consumer_tag: String,
    /// Bumped per channel: tags of an older channel are stale.
    epoch: u64,
    held: Vec<Held>,
}

impl RabbitConsumer {
    async fn attach(&mut self) -> MsgResult<()> {
        let ch = self.broker.connection().await?.channel().await?;
        ch.qos(u16::try_from(self.sub.max_in_flight).unwrap_or(u16::MAX)).await?;
        self.consumer_tag = ch.consume(&self.queue, "", false, false, &Vec::new()).await?;
        self.chan = Some(ch);
        self.epoch += 1;
        // What the old channel held went back to the queue with it.
        self.held.clear();
        Ok(())
    }

    fn parse_tag(&self, d: &Delivery) -> Option<u64> {
        let (epoch, tag) = d.tag.split_once(':')?;
        let (epoch, tag): (u64, u64) = (epoch.parse().ok()?, tag.parse().ok()?);
        // Stale (older channel) or requeued after its ack wait: nothing to settle.
        (epoch == self.epoch && self.held.iter().any(|h| h.tag == tag)).then_some(tag)
    }

    /// Settle and wait until the ack / nack is on the wire: once this
    /// returns, only a connection lost while it travels can bring the
    /// delivery back (then as a redelivery — the contract's at-least-once).
    async fn settle(&mut self, d: &Delivery, what: Option<bool>) -> MsgResult<()> {
        let Some(tag) = self.parse_tag(d) else { return Ok(()) };
        self.held.retain(|h| h.tag != tag);
        let Some(ch) = self.chan.as_ref() else { return Ok(()) };
        let r = match what {
            None => ch.ack(tag, false),
            Some(requeue) => ch.nack(tag, false, requeue),
        };
        let r = match r {
            Ok(()) => ch.flush().await,
            Err(e) => Err(e),
        };
        // A lost channel requeues it anyway.
        if r.is_err() {
            self.chan = None;
        }
        Ok(())
    }
}

impl Consumer for RabbitConsumer {
    async fn next(&mut self) -> MsgResult<Delivery> {
        loop {
            if self.chan.as_ref().is_none_or(|c| !c.is_open()) {
                match self.attach().await {
                    Ok(()) => {}
                    Err(e @ (MsgError::Auth(_) | MsgError::Config(_))) => return Err(e),
                    // 404: the queue is gone (topic deleted) — not healed by retrying.
                    Err(MsgError::Broker(m)) if m.contains("404") => return Err(MsgError::Broker(m)),
                    Err(_) => {
                        rt_core::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                }
            }
            // Held past the ack wait: back to the queue (redelivered, to anyone).
            let now = Instant::now();
            let ack_wait = self.sub.ack_wait;
            let expired: Vec<u64> = self.held.iter().filter(|h| now.duration_since(h.since) >= ack_wait).map(|h| h.tag).collect();
            if let Some(ch) = self.chan.as_ref() {
                for tag in &expired {
                    let _ = ch.nack(*tag, false, true);
                }
            }
            self.held.retain(|h| !expired.contains(&h.tag));
            let wait = self
                .held
                .iter()
                .map(|h| (h.since + ack_wait).saturating_duration_since(now))
                .min()
                .unwrap_or(Duration::from_millis(500))
                .clamp(Duration::from_millis(5), Duration::from_millis(500));
            let Some(ch) = self.chan.as_ref() else { continue };
            if ch.take_cancelled().contains(&self.consumer_tag) {
                return Err(MsgError::Broker(format!("the broker cancelled the consumer of {} (queue deleted?)", self.queue)));
            }
            let d = match ch.next_delivery(Some(wait)).await {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(_) => {
                    self.chan = None;
                    continue;
                }
            };
            self.held.push(Held { tag: d.delivery_tag, since: Instant::now() });
            let (headers, key, count) = from_table(d.props.headers);
            return Ok(Delivery {
                topic: d.exchange,
                key,
                payload: d.body,
                headers,
                redelivered: d.redelivered || count > 0,
                tag: format!("{}:{}", self.epoch, d.delivery_tag),
            });
        }
    }

    async fn ack(&mut self, d: &Delivery) -> MsgResult<()> {
        self.settle(d, None).await
    }

    async fn nack(&mut self, d: &Delivery, requeue: bool) -> MsgResult<()> {
        self.settle(d, Some(requeue)).await
    }

    async fn close(mut self) -> MsgResult<()> {
        if let Some(ch) = self.chan.take() {
            if ch.is_open() {
                let _ = ch.cancel(&self.consumer_tag).await;
            }
            // Closing the channel requeues what it still held.
            ch.close().await?;
        }
        Ok(())
    }
}

impl Broker for RabbitBroker {
    type Producer = RabbitProducer;
    type Consumer = RabbitConsumer;

    async fn producer(&self) -> MsgResult<RabbitProducer> {
        Ok(RabbitProducer { broker: self.clone(), chan: RefCell::new(None), seq: Cell::new(0) })
    }

    async fn subscribe(&self, sub: Subscription) -> MsgResult<RabbitConsumer> {
        self.ensure_topic(&sub.topic).await?;
        let queue = Self::queue_name(&sub.topic, &sub.group);
        let mut args: Table = vec![("x-queue-type".into(), Field::str("quorum"))];
        if let Some(dl) = &sub.dead_letter {
            self.ensure_topic(dl).await?;
            args.push(("x-dead-letter-exchange".into(), Field::str(dl)));
            args.push(("x-dead-letter-strategy".into(), Field::str("at-least-once")));
            args.push(("x-overflow".into(), Field::str("reject-publish")));
        }
        let declared = {
            let (q, a) = (queue.clone(), args.clone());
            self.with_channel(|ch| async move {
                let r = ch.queue_declare(&q, true, false, false, &a).await;
                (ch, r)
            })
            .await
        };
        match declared {
            Ok(_) => {}
            // It exists with other settings (another dead-letter topic …): join it as it is.
            Err(MsgError::Broker(m)) if m.contains("406") => {
                let q = queue.clone();
                self.with_channel(|ch| async move {
                    let r = ch.queue_inspect(&q).await;
                    (ch, r)
                })
                .await?;
            }
            Err(e) => return Err(e),
        }
        {
            let (q, t) = (queue.clone(), sub.topic.clone());
            self.with_channel(|ch| async move {
                let r = ch.queue_bind(&q, &t, "", &Vec::new()).await;
                (ch, r)
            })
            .await?;
        }
        self.inner.groups.borrow_mut().entry(sub.topic.clone()).or_default().insert(queue.clone());
        let mut c = RabbitConsumer { broker: self.clone(), sub, queue, chan: None, consumer_tag: String::new(), epoch: 0, held: Vec::new() };
        c.attach().await?;
        Ok(c)
    }

    async fn ensure_topic(&self, topic: &str) -> MsgResult<()> {
        let t = topic.to_string();
        self.with_channel(|ch| async move {
            let r = ch.exchange_declare(&t, "fanout", true, false, &Vec::new()).await;
            (ch, r)
        })
        .await
    }

    async fn delete_topic(&self, topic: &str) -> MsgResult<()> {
        let queues: Vec<String> = self.inner.groups.borrow_mut().remove(topic).map(|s| s.into_iter().collect()).unwrap_or_default();
        let t = topic.to_string();
        self.with_channel(|ch| async move {
            for q in &queues {
                if let Err(e) = ch.queue_delete(q, false, false).await {
                    return (ch, Err(e));
                }
            }
            let r = ch.exchange_delete(&t, false).await;
            (ch, r)
        })
        .await
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { replay: false, ack_timeout: true, dead_letter: true, key_order_across_members: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_keys_and_repeats_map_to_tables() {
        let h = vec![("a".to_string(), "1".to_string()), ("m".to_string(), "x".to_string()), ("m".to_string(), "y".to_string())];
        let t = to_table(&h, Some(&[0, 255]));
        assert_eq!(t.len(), 3);
        assert_eq!(t[1].1, Field::Array(vec![Field::str("x"), Field::str("y")]));
        let mut t2 = t.clone();
        t2.push(("x-delivery-count".into(), Field::I64(2)));
        t2.push(("x-death".into(), Field::Array(vec![Field::Table(vec![])])));
        let (back, key, count) = from_table(Some(t2));
        assert_eq!(&back[..3], &h[..]);
        assert_eq!((key, count), (Some(vec![0, 255]), 2));
    }
}
