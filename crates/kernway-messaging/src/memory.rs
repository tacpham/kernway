//! An in-process broker (one shard): topics are logs, groups are positions
//! with in-flight deliveries. For tests and single-node use; it passes the
//! same conformance suite as the network brokers.

use crate::{Broker, Capabilities, Consumer, Delivery, MsgError, MsgResult, OutMessage, Producer, Receipt, StartAt, Subscription};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

struct Stored {
    msg: OutMessage,
}

#[derive(Default)]
struct Group {
    /// Next log position never delivered.
    next: usize,
    /// In flight: position → (consumer id, redelivery deadline).
    pending: BTreeMap<usize, (u64, Instant)>,
    /// Positions to deliver again, in order.
    again: VecDeque<usize>,
    /// Positions delivered at least once.
    seen: std::collections::HashSet<usize>,
}

#[derive(Default)]
struct Topic {
    log: Vec<Stored>,
    groups: HashMap<String, Group>,
}

#[derive(Default)]
struct State {
    topics: HashMap<String, Topic>,
    next_consumer: u64,
}

/// The in-process broker. Clones share state.
#[derive(Clone, Default)]
pub struct MemoryBroker {
    state: Rc<RefCell<State>>,
}

impl MemoryBroker {
    /// An empty broker.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Publishes into a [`MemoryBroker`].
pub struct MemoryProducer {
    state: Rc<RefCell<State>>,
}

impl Producer for MemoryProducer {
    async fn publish(&self, topic: &str, msg: OutMessage) -> MsgResult<Receipt> {
        let mut st = self.state.borrow_mut();
        let t = st.topics.entry(topic.to_string()).or_default();
        t.log.push(Stored { msg });
        Ok(Receipt { id: (t.log.len() - 1).to_string() })
    }
}

/// A group member of a [`MemoryBroker`].
pub struct MemoryConsumer {
    state: Rc<RefCell<State>>,
    sub: Subscription,
    id: u64,
    /// Positions this consumer holds (in flight).
    held: Vec<usize>,
}

impl MemoryConsumer {
    fn try_next(&mut self) -> Option<Delivery> {
        let mut st = self.state.borrow_mut();
        let t = st.topics.entry(self.sub.topic.clone()).or_default();
        let g = t.groups.get_mut(&self.sub.group)?;
        // Past their deadline: deliverable again.
        let now = Instant::now();
        let expired: Vec<usize> = g.pending.iter().filter(|(_, (_, d))| *d <= now).map(|(p, _)| *p).collect();
        for p in expired {
            g.pending.remove(&p);
            g.again.push_back(p);
        }
        if self.held.len() >= self.sub.max_in_flight {
            return None;
        }
        // Redeliveries first (in order), then new messages — but never a
        // message whose key has an earlier message still in flight.
        let blocked = |log: &[Stored], pending: &BTreeMap<usize, (u64, Instant)>, pos: usize| {
            let key = &log[pos].msg.key;
            key.is_some() && pending.keys().any(|&q| q < pos && &log[q].msg.key == key)
        };
        let mut pick = None;
        if let Some(i) = g.again.iter().position(|&p| !blocked(&t.log, &g.pending, p)) {
            pick = g.again.remove(i);
        } else if g.next < t.log.len() && !blocked(&t.log, &g.pending, g.next) {
            pick = Some(g.next);
            g.next += 1;
        }
        let pos = pick?;
        let redelivered = !g.seen.insert(pos);
        g.pending.insert(pos, (self.id, now + self.sub.ack_wait));
        self.held.push(pos);
        let m = &t.log[pos].msg;
        Some(Delivery {
            topic: self.sub.topic.clone(),
            key: m.key.clone(),
            payload: m.payload.clone(),
            headers: m.headers.clone(),
            redelivered,
            tag: pos.to_string(),
        })
    }

    fn settle(&mut self, d: &Delivery, again: Option<bool>) -> MsgResult<()> {
        let pos: usize = d.tag.parse().map_err(|_| MsgError::Protocol(format!("bad delivery tag {:?}", d.tag)))?;
        self.held.retain(|&p| p != pos);
        let dead = {
            let mut st = self.state.borrow_mut();
            let t = st.topics.get_mut(&self.sub.topic).ok_or_else(|| MsgError::Broker("topic deleted".into()))?;
            let g = t.groups.get_mut(&self.sub.group).ok_or_else(|| MsgError::Broker("group deleted".into()))?;
            match g.pending.get(&pos) {
                Some((owner, _)) if *owner == self.id => {
                    g.pending.remove(&pos);
                }
                // Redelivered elsewhere meanwhile (ack wait passed): this
                // settlement is late; the newer delivery decides.
                _ => return Ok(()),
            }
            match again {
                None => None,
                Some(true) => {
                    g.again.push_front(pos);
                    None
                }
                Some(false) => self.sub.dead_letter.clone().map(|dl| (dl, t.log[pos].msg.clone())),
            }
        };
        if let Some((topic, msg)) = dead {
            let mut st = self.state.borrow_mut();
            st.topics.entry(topic).or_default().log.push(Stored { msg });
        }
        Ok(())
    }
}

impl Consumer for MemoryConsumer {
    async fn next(&mut self) -> MsgResult<Delivery> {
        loop {
            if let Some(d) = self.try_next() {
                return Ok(d);
            }
            rt_core::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn ack(&mut self, d: &Delivery) -> MsgResult<()> {
        self.settle(d, None)
    }

    async fn nack(&mut self, d: &Delivery, requeue: bool) -> MsgResult<()> {
        self.settle(d, Some(requeue))
    }

    async fn close(mut self) -> MsgResult<()> {
        self.release();
        Ok(())
    }
}

impl MemoryConsumer {
    fn release(&mut self) {
        let held = std::mem::take(&mut self.held);
        let mut st = self.state.borrow_mut();
        if let Some(g) = st.topics.get_mut(&self.sub.topic).and_then(|t| t.groups.get_mut(&self.sub.group)) {
            for p in held {
                if g.pending.get(&p).is_some_and(|(o, _)| *o == self.id) {
                    g.pending.remove(&p);
                    g.again.push_back(p);
                }
            }
            let mut v: Vec<usize> = g.again.drain(..).collect();
            v.sort_unstable();
            g.again.extend(v);
        }
    }
}

impl Drop for MemoryConsumer {
    fn drop(&mut self) {
        self.release();
    }
}

impl Broker for MemoryBroker {
    type Producer = MemoryProducer;
    type Consumer = MemoryConsumer;

    async fn producer(&self) -> MsgResult<MemoryProducer> {
        Ok(MemoryProducer { state: self.state.clone() })
    }

    async fn subscribe(&self, sub: Subscription) -> MsgResult<MemoryConsumer> {
        let id = {
            let mut st = self.state.borrow_mut();
            st.next_consumer += 1;
            let id = st.next_consumer;
            let t = st.topics.entry(sub.topic.clone()).or_default();
            let len = t.log.len();
            t.groups.entry(sub.group.clone()).or_insert_with(|| Group { next: if sub.start == StartAt::Earliest { 0 } else { len }, ..Default::default() });
            id
        };
        Ok(MemoryConsumer { state: self.state.clone(), sub, id, held: Vec::new() })
    }

    async fn ensure_topic(&self, topic: &str) -> MsgResult<()> {
        self.state.borrow_mut().topics.entry(topic.to_string()).or_default();
        Ok(())
    }

    async fn delete_topic(&self, topic: &str) -> MsgResult<()> {
        self.state.borrow_mut().topics.remove(topic);
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities { replay: true, ack_timeout: true, dead_letter: true, key_order_across_members: true }
    }
}
