//! # kernway-orm-core
//!
//! The ORM spec: traits and metadata types, no driver. Roughly what JPA
//! (JSR-338) is to Hibernate.

//!
//! ## The idea
//!
//! This crate defines *what* persistence looks like; a backend crate decides
//! *how*. [`kernway-orm-sqlite`] renders SQL, [`kernway-orm-memory`] keeps a
//! `HashMap`, and a community driver for Postgres or Mongo implements the same
//! traits without asking anyone's permission.
//!
//! Two consequences follow, and both are the point:
//!
//! - **Your service code does not name a database.** It depends on
//!   `Repository<User>`. Switching backends is a dependency change, not a
//!   rewrite.
//! - **Tests do not need a database.** Swap the in-memory backend in and the
//!   code under test cannot tell.
//!
//! The crate is standalone — it pulls in `thiserror` and nothing else, and it
//! does not depend on the rest of Kernway. Use the ORM without the web stack.
//!
//! [`kernway-orm-sqlite`]: https://docs.rs/kernway-orm-sqlite
//! [`kernway-orm-memory`]: https://docs.rs/kernway-orm-memory
//!
//! ## How the pieces fit
//!
//! ```text
//!   #[entity]  ──compile time──►  impl Entity for User
//!   struct User                     table_name() · id() · columns()
//!                                              │
//!                                              │  the backend reads this
//!                                              ▼
//!             Repository<User>  ──────►  a driver (SQL / in-memory / ...)
//!                    │
//!                    │ .query()
//!                    ▼
//!             QueryBuilder<User>   filter_* · order_by_* · limit · offset · with
//!                    │
//!                    │ terminal call — nothing runs before this
//!                    ▼
//!             Vec<User> │ Option<User> │ u64 │ Page<User>
//! ```
//!
//! [`Entity`] carries no data of its own: it is pure metadata the macro derives
//! from your struct, which is how a backend can build SQL for a type it has
//! never seen.
//!
//! ## Two deliberate departures from JPA
//!
//! **No lazy loading.** Hibernate returns proxy objects that fire a query when
//! you touch a field. Rust has no runtime bytecode generation, so that trick is
//! unavailable — and its absence is an improvement. Relations load when you ask
//! via [`QueryBuilder::with`], and never otherwise, so an N+1 cannot appear
//! behind your back.
//!
//! **No `EntityManager`.** It is stateful by design — first-level cache, dirty
//! tracking, identity map — which sits badly with ownership. [`Repository<T>`]
//! is stateless, and change tracking is replaced by an explicit `save`.
//!
//! [`QueryBuilder::with`]: query::QueryBuilder::with
//! [`Repository<T>`]: repository::Repository
//!
//! ## Async trait surface
//!
//! Repository methods return boxed futures, while backends decide how to run
//! their work: purely in-memory implementations can resolve immediately and
//! blocking drivers can hop to a blocking pool. The spec stays runtime-agnostic
//! by exposing only [`BoxFuture`].
//!
//! ## Module map
//!
//! - [`entity`] — [`Entity`], [`ColumnDef`], [`ColumnType`]: the mapping metadata
//! - [`repository`] — [`Repository`]: CRUD plus the query entry point
//! - [`query`] — [`QueryBuilder`]: the fluent, lazily-executed query API
//! - [`page`] — [`Page`]: a page of results with its counts
//! - [`error`] — [`OrmError`]: the failure vocabulary every backend maps onto

use std::future::Future;
use std::pin::Pin;

/// Async future alias used by all ORM traits.
/// `'a` is the lifetime of the borrowed receiver.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// SQL syntax adapters shared by relational backends.
pub mod dialect;
/// The entry-point driver abstraction implemented by backends.
pub mod driver;
/// Entity mapping metadata: what a struct is called in the database.
pub mod entity;
/// The error type shared by every backend.
pub mod error;
/// Paginated results.
pub mod page;
/// The fluent query builder contract.
pub mod query;
/// The CRUD contract a backend implements per entity type.
pub mod repository;

pub use dialect::SqlDialect;
pub use driver::{Driver, DriverCapabilities};
pub use entity::{ColumnDef, ColumnType, Entity};
pub use error::OrmError;
pub use page::Page;
pub use query::QueryBuilder;
pub use repository::Repository;
