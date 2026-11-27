//! # kernway-rabbitmq
//!
//! A self-written async AMQP 0-9-1 client for RabbitMQ on the Kernway
//! reactor — connections with TLS / mutual TLS, PLAIN / AMQPLAIN / EXTERNAL
//! auth, heartbeats, channels, exchanges / queues / bindings, publisher
//! confirms and returns, consumers with prefetch, ack / nack / get, and
//! broker-initiated closes and cancels — and the
//! [`kernway_messaging::Broker`] backend [`RabbitBroker`] (topics as fanout
//! exchanges, groups as quorum queues), which passes the messaging
//! conformance suite.
//!
//! ```rust,ignore
//! let bus = RabbitBroker::connect_uri("amqp://kw:kw_pw@127.0.0.1:5672/%2F").await?;
//! let mut c = bus.subscribe(Subscription::new("pos.checkins", "front-desk")).await?;
//! bus.producer().await?.publish("pos.checkins", OutMessage::new(json)).await?;
//! let d = c.next().await?;
//! handle(&d.payload);
//! c.ack(&d).await?;
//! ```
//!
//! Everything is `!Send`: one connection per shard.

pub mod broker;
pub mod codec;
pub mod conn;
pub mod options;

pub use broker::{RabbitBroker, RabbitConsumer, RabbitProducer};
pub use codec::{Field, Properties, Table};
pub use conn::{Channel, Connection, Delivered, Returned};
pub use kernway_messaging::{Broker, Consumer, Delivery, MsgError, OutMessage, Producer, Subscription};
pub use options::{Mechanism, Options, TlsOptions};
