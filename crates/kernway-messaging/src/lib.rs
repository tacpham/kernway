//! # kernway-messaging
//!
//! The message-bus contract every broker backend implements — NATS
//! JetStream, RabbitMQ, Kafka — written once, with a conformance suite
//! ([`conformance`], feature `conformance`) each backend must pass against a
//! live broker, and an in-process broker ([`memory`]) for tests and
//! single-node use.
//!
//! Where `kernway-cache-core::pubsub` is fire-and-forget fan-out (a lost
//! message is fine: SSE / WebSocket push), this is **durable, at-least-once
//! delivery**:
//!
//! - [`Producer::publish`] returns once the broker has stored the message
//!   (publisher confirm / ack / produce ack) — a returned `Ok` is never lost.
//! - Consumers read as a **group** ([`Subscription::group`]): each message
//!   goes to one member of the group; every group gets every message.
//! - A delivery is [`ack`](Consumer::ack)ed when handled. One that is
//!   [`nack`](Consumer::nack)ed, or left unacknowledged when its consumer
//!   goes away, is delivered again (with [`Delivery::redelivered`] set) —
//!   handlers must be idempotent. A rejected one goes to the subscription's
//!   dead-letter topic when it has one.
//! - Messages with the same [`OutMessage::key`] reach a member in the order
//!   they were published. Across the members of a group that holds only
//!   where the backend gives a key to one member at a time
//!   ([`Capabilities::key_order_across_members`]: Kafka partitions, the
//!   in-process broker) — elsewhere two members may handle one key's
//!   messages at the same time.
//!
//! Everything is `!Send`, like the rest of the thread-per-core stack:
//! producers and consumers live on the shard that opened them.

use std::future::Future;
use std::time::Duration;

#[cfg(feature = "conformance")]
pub mod conformance;
pub mod memory;

/// A message to publish.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutMessage {
    /// Ordering / partitioning key: messages with the same key stay in order.
    pub key: Option<Vec<u8>>,
    /// The payload (bytes: callers choose the encoding).
    pub payload: Vec<u8>,
    /// Headers, in order (names may repeat).
    pub headers: Vec<(String, String)>,
}

impl OutMessage {
    /// A message with just a payload.
    pub fn new(payload: impl Into<Vec<u8>>) -> Self {
        OutMessage { key: None, payload: payload.into(), headers: Vec::new() }
    }

    /// With an ordering key.
    pub fn key(mut self, key: impl Into<Vec<u8>>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// With a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// What the broker said about a stored message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Receipt {
    /// The broker's position / id for it (stream sequence, partition:offset,
    /// delivery tag …), as text; empty when the broker gives none.
    pub id: String,
}

/// A delivered message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The topic it was published to.
    pub topic: String,
    /// Its key.
    pub key: Option<Vec<u8>>,
    /// The payload.
    pub payload: Vec<u8>,
    /// Headers.
    pub headers: Vec<(String, String)>,
    /// Delivered before (and not acknowledged).
    pub redelivered: bool,
    /// The backend's handle for acknowledging it (opaque to callers).
    pub tag: String,
}

impl Delivery {
    /// The first header named `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Where a new group starts reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartAt {
    /// From the oldest stored message.
    Earliest,
    /// Only messages published after the group was created.
    #[default]
    Latest,
}

/// A group subscription to a topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// The topic.
    pub topic: String,
    /// The consumer group: members share the work; groups each see everything.
    pub group: String,
    /// Where a group that has no position yet starts.
    pub start: StartAt,
    /// How long a delivery may stay unacknowledged before it is delivered
    /// again (to any member).
    pub ack_wait: Duration,
    /// Deliveries handed to this consumer before it acknowledges any.
    pub max_in_flight: usize,
    /// Rejected deliveries go here (a topic), when set.
    pub dead_letter: Option<String>,
}

impl Subscription {
    /// Group `group` on `topic`, from the latest message, 30 s ack wait.
    pub fn new(topic: impl Into<String>, group: impl Into<String>) -> Self {
        Subscription {
            topic: topic.into(),
            group: group.into(),
            start: StartAt::Latest,
            ack_wait: Duration::from_secs(30),
            max_in_flight: 64,
            dead_letter: None,
        }
    }

    /// Start from the earliest message.
    pub fn from_earliest(mut self) -> Self {
        self.start = StartAt::Earliest;
        self
    }

    /// With an ack wait.
    pub fn ack_wait(mut self, d: Duration) -> Self {
        self.ack_wait = d;
        self
    }

    /// With a dead-letter topic.
    pub fn dead_letter(mut self, topic: impl Into<String>) -> Self {
        self.dead_letter = Some(topic.into());
        self
    }

    /// With a prefetch limit.
    pub fn max_in_flight(mut self, n: usize) -> Self {
        self.max_in_flight = n.max(1);
        self
    }
}

/// Why a messaging call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MsgError {
    /// Bad configuration (URL, options).
    Config(String),
    /// Connection lost / refused / timed out.
    Connection(String),
    /// Authentication refused.
    Auth(String),
    /// The broker refused the request.
    Broker(String),
    /// Nothing arrived within the wait (see [`Consumer::next_timeout`]).
    Timeout,
    /// A protocol violation.
    Protocol(String),
}

impl std::fmt::Display for MsgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MsgError::Config(m) => write!(f, "messaging configuration: {m}"),
            MsgError::Connection(m) => write!(f, "messaging connection: {m}"),
            MsgError::Auth(m) => write!(f, "messaging authentication: {m}"),
            MsgError::Broker(m) => write!(f, "broker error: {m}"),
            MsgError::Timeout => write!(f, "no message within the wait"),
            MsgError::Protocol(m) => write!(f, "messaging protocol: {m}"),
        }
    }
}

impl std::error::Error for MsgError {}

/// `Result` with [`MsgError`].
pub type MsgResult<T> = Result<T, MsgError>;

/// Sends messages; every `Ok` means the broker stored it.
pub trait Producer {
    /// Publish `msg` to `topic`, waiting for the broker's confirmation.
    fn publish(&self, topic: &str, msg: OutMessage) -> impl Future<Output = MsgResult<Receipt>>;
}

/// A group member receiving deliveries.
pub trait Consumer {
    /// The next delivery, waiting for one.
    fn next(&mut self) -> impl Future<Output = MsgResult<Delivery>>;

    /// The next delivery, or [`MsgError::Timeout`] after `wait`.
    fn next_timeout(&mut self, wait: Duration) -> impl Future<Output = MsgResult<Delivery>> {
        async move {
            match rt_core::timeout(wait, self.next()).await {
                Ok(r) => r,
                Err(_) => Err(MsgError::Timeout),
            }
        }
    }

    /// The delivery was handled: it is not delivered again.
    fn ack(&mut self, d: &Delivery) -> impl Future<Output = MsgResult<()>>;

    /// The delivery was not handled: deliver it again (`requeue`), or reject
    /// it — to the dead-letter topic when the subscription has one.
    fn nack(&mut self, d: &Delivery, requeue: bool) -> impl Future<Output = MsgResult<()>>;

    /// Leave the group; unacknowledged deliveries go to other members.
    fn close(self) -> impl Future<Output = MsgResult<()>>
    where
        Self: Sized;
}

/// What a backend can do beyond the contract's core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// A new group can start from messages published before it existed
    /// ([`StartAt::Earliest`]).
    pub replay: bool,
    /// Deliveries left unacknowledged past `ack_wait` are redelivered while
    /// their consumer is still connected.
    pub ack_timeout: bool,
    /// Rejected deliveries reach a dead-letter topic.
    pub dead_letter: bool,
    /// A key's next message is not handed to any member while an earlier
    /// one of that key is still unacknowledged in the group.
    pub key_order_across_members: bool,
}

/// A broker: makes producers and group consumers.
pub trait Broker {
    /// Its producer.
    type Producer: Producer;
    /// Its consumer.
    type Consumer: Consumer;

    /// A producer.
    fn producer(&self) -> impl Future<Output = MsgResult<Self::Producer>>;

    /// Join `sub.group` on `sub.topic` (creating topic and group as needed).
    fn subscribe(&self, sub: Subscription) -> impl Future<Output = MsgResult<Self::Consumer>>;

    /// Create `topic` if it does not exist (publishing to a topic nobody
    /// created is otherwise backend-defined).
    fn ensure_topic(&self, topic: &str) -> impl Future<Output = MsgResult<()>>;

    /// Remove `topic` and its messages (tests, tear-down).
    fn delete_topic(&self, topic: &str) -> impl Future<Output = MsgResult<()>>;

    /// What this backend supports.
    fn capabilities(&self) -> Capabilities;
}
