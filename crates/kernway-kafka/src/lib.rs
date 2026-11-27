//! # kernway-kafka
//!
//! A self-written async Kafka client on the Kernway reactor — the binary
//! protocol (flexible versions accepted by Kafka 3.x and 4.x, checked with
//! ApiVersions), record batches v2 (CRC-32C; gzip / snappy read), TLS and
//! mutual TLS, SASL PLAIN / SCRAM-SHA-256 / SCRAM-SHA-512, cluster metadata
//! with leader routing, an idempotent producer (acks=all, the Java client's
//! murmur2 partitioner), fetches, offsets, topic administration, and
//! consumer groups with client-side assignment and committed offsets — and
//! the [`kernway_messaging::Broker`] backend [`KafkaBroker`], which passes
//! the messaging conformance suite with every capability.
//!
//! ```rust,ignore
//! let bus = KafkaBroker::connect_url("kafka://kw:kw_pw@127.0.0.1:9093?sasl=scram-sha-512").await?;
//! bus.ensure_topic("pos.checkins").await?;
//! let mut c = bus.subscribe(Subscription::new("pos.checkins", "front-desk")).await?;
//! bus.producer().await?.publish("pos.checkins", OutMessage::new(json).key(ticket_id)).await?;
//! let d = c.next().await?;
//! handle(&d.payload);
//! c.ack(&d).await?;
//! ```
//!
//! Everything is `!Send`: one client per shard. Requests that wait on the
//! broker (group joins, fetch long-polls) use connections of their own — a
//! broker answers one request per connection at a time.


pub mod broker;
pub mod client;
pub mod conn;
pub mod options;
pub mod protocol;
pub mod records;

pub use broker::{KafkaBroker, KafkaConsumer, KafkaProducer};
pub use client::Client;
pub use kernway_messaging::{Broker, Consumer, Delivery, MsgError, OutMessage, Producer, Subscription};
pub use options::{Options, Sasl, TlsOptions};
