//! # kernway-orm-mongodb
//!
//! MongoDB for Kernway — a **self-written driver** on the per-core async
//! reactor (no `mongodb` crate, no tokio), and an ORM backend:
//!
//! - [`bson`]: the value model, binary codec, Extended JSON, serde bridge;
//! - `OP_MSG` with document sequences, snappy (written here) / zlib compression;
//! - TCP / TLS / Unix sockets, `mongodb+srv://` with a self-written DNS client;
//! - SCRAM-SHA-256 / SHA-1 (speculative), X.509, PLAIN;
//! - topology discovery and monitoring, server selection (read preferences,
//!   tags, staleness), per-server pools, failover;
//! - sessions, causal consistency, snapshot reads, transactions
//!   (`with_transaction`), retryable reads and writes;
//! - CRUD, cursors, aggregation, bulk writes, indexes, change streams
//!   (resumable), GridFS;
//! - [`MongoDriver`]: the kernway ORM over documents.
//!
//! ```rust,ignore
//! let client = Client::connect("mongodb://127.0.0.1:27017,127.0.0.1:27018/?replicaSet=rs0").await?;
//! let items = client.database("shop").collection::<Item>("items");
//! items.insert_one(&item).await?;
//! let cheap = items.find(doc! { "price" => doc! { "$lt" => 10 } }, FindOptions::default()).await?.collect().await?;
//! ```

pub mod bson;
mod client;
mod client_bulk;
mod collection;
pub mod conn;
mod dns;
pub mod error;
pub mod events;
mod gridfs;
pub mod options;
mod ordered_json;
pub mod orm;
pub mod topology;
mod wire;

pub use bson::{from_bson, from_document, to_document, Binary, Bson, DateTime, Decimal128, Document, ObjectId, Regex, Timestamp};
pub use client::{Client, Cursor, Session, SessionOptions, TransactionOptions};
pub use client_bulk::{ClientBulkWriteFailure, ClientBulkWriteOptions, ClientBulkWriteResult, ClientUpdateResult, ClientWriteModel, VerboseResults};
pub use collection::{
    AggregateOptions, BulkWriteOptions, BulkWriteResult, ChangeStream, ChangeStreamOptions, Collection, CountOptions, Database, DeleteOptions,
    DistinctOptions, EstimatedCountOptions, FindOneAndModifyOptions, FindOptions, IndexModel, InsertManyResult, InsertOptions, SearchIndexModel, UpdateOptions,
    UpdateResult, WriteModel,
};
pub use error::{Error, ErrorKind, Result};
pub use events::{CommandContext, CommandEvent};
pub use gridfs::{GridFsBucket, DEFAULT_CHUNK_SIZE};
pub use orm::{MongoDriver, MongoTx};
pub use options::{
    Address, AuthMechanism, ClientOptions, Compressor, Credential, ReadConcern, ReadMode, ReadPreference, TlsOptions, WriteConcern,
};
