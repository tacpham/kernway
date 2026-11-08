//! Publish / subscribe — the message-bus abstraction, backend-agnostic.
//!
//! Where [`Cache`](crate::Cache) is request/reply key-value, pub/sub is a
//! one-to-many **stream**: a publisher sends to a channel, every current
//! subscriber receives. It is the backbone of cross-instance fan-out — a POS
//! kiosk check-in on one backend node reaching the front-desk screen on another
//! goes through here (typically Redis), then out to clients over WebSocket/SSE.
//!
//! The surface is **async and `!Send`**, matching the thread-per-core runtime: a
//! subscription owns a live connection pinned to its shard, and a publisher is
//! created per shard from cheap config. Backends live in their own crates
//! (`kernway-redis` for Redis); this crate only defines the contract and an
//! in-process broker is a later addition.
//!
//! ```rust,ignore
//! // fan out a check-in to every node, then on to clients
//! bus.publish("location:42:queue", &json).await?;
//!
//! let mut sub = bus.subscribe(&["location:42:queue"]).await?;
//! while let Ok(msg) = sub.recv().await {
//!     push_to_websocket_clients(&msg.payload).await;
//! }
//! ```

use crate::error::CacheError;
use std::future::Future;
use std::pin::Pin;

/// Boxed future used by the pub/sub traits. **Not `Send`** — a subscription is
/// pinned to the shard that opened it (see the module docs).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A message delivered to a subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The channel it arrived on (useful when subscribed to several).
    pub channel: String,
    /// The raw payload — bytes, so callers choose their own encoding.
    pub payload: Vec<u8>,
}

/// Send messages to channels.
///
/// A publisher holds cheap config (address/credentials/pool), not a live
/// subscription, so it can be recreated per shard. Backends may keep one
/// connection internally; concurrent publishes on a single handle are not
/// required to be supported — make another handle for that.
pub trait Publisher {
    /// Publish `payload` to `channel`. Returns how many subscribers received it
    /// (0 is normal — pub/sub is fire-and-forget and not durable).
    fn publish<'a>(&'a self, channel: &'a str, payload: &'a [u8]) -> BoxFuture<'a, Result<u64, CacheError>>;
}

/// A live subscription: await messages one at a time.
///
/// Dropping it unsubscribes (the backend closes its connection).
pub trait Subscription {
    /// Await the next message on the subscribed channels.
    fn recv(&mut self) -> BoxFuture<'_, Result<Message, CacheError>>;
}

/// Open subscriptions to one or more channels.
pub trait Subscriber {
    /// Subscribe to `channels` and return a stream of their messages. The
    /// returned subscription owns its own connection (a subscribed connection
    /// cannot issue other commands).
    fn subscribe<'a>(
        &'a self,
        channels: &'a [&'a str],
    ) -> BoxFuture<'a, Result<Box<dyn Subscription + 'a>, CacheError>>;
}

/// A full pub/sub backend — both ends. Blanket-implemented for any type that is
/// both a [`Publisher`] and a [`Subscriber`].
pub trait PubSub: Publisher + Subscriber {}
impl<T: Publisher + Subscriber> PubSub for T {}
