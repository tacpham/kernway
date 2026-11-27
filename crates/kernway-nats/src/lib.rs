//! # kernway-nats
//!
//! A self-written async NATS client on the Kernway reactor — core NATS
//! (publish / subscribe / queue groups / request-reply, headers, TLS incl.
//! `tls_first` and mutual TLS, user/password, token, NKey and JWT
//! credentials, cluster discovery, automatic reconnect with resubscribe) and
//! JetStream (streams, durable pull consumers, confirmed publishes with
//! de-duplication, double acks) — and the [`kernway_messaging::Broker`]
//! backend [`NatsBroker`], which passes the messaging conformance suite.
//!
//! ```rust,ignore
//! let bus = NatsBroker::connect_url("nats://127.0.0.1:4222").await?;
//! bus.ensure_topic("pos.checkins").await?;
//! bus.producer().await?.publish("pos.checkins", OutMessage::new(json)).await?;
//! let mut c = bus.subscribe(Subscription::new("pos.checkins", "front-desk")).await?;
//! let d = c.next().await?;
//! handle(&d.payload);
//! c.ack(&d).await?;
//! ```
//!
//! Everything is `!Send`: one client per shard.

pub mod client;
pub mod jetstream;
pub mod options;

pub use client::{Client, Msg, Subscriber};
pub use jetstream::{parse_ack_subject, AckInfo, JetStream, NatsBroker, NatsConsumer, NatsProducer};
pub use kernway_messaging::{Broker, Consumer, Delivery, MsgError, OutMessage, Producer, Subscription};
pub use options::{Auth, Options, TlsOptions};
