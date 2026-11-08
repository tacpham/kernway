//! Transactions — run a unit of work atomically, with row-level locking.
//!
//! The base [`Driver`](crate::Driver) / [`Repository`](crate::Repository) surface
//! is transaction-free: each `save` / `delete` is its own autocommit statement.
//! That is enough for most CRUD, but **not** for money: a checkout or a turn
//! assignment must read a row, decide, and write it back with no other writer
//! slipping in between.
//!
//! This module adds that as an **optional capability**. A backend opts in by
//! implementing [`TransactionalDriver`]; drivers that cannot (the in-memory and
//! search backends) are unaffected.
//!
//! # Shape: an async closure
//!
//! The closure receives an owned transaction handle and returns a future. The
//! handle holds one connection for the whole transaction, so a row locked via
//! [`TxRepository::find_by_id_for_update`] stays locked until the future
//! resolves. Commit is automatic on `Ok`; any `Err` rolls back.
//!
//! ```rust,ignore
//! let total = db.transaction(|tx| Box::pin(async move {
//!     let tickets = tx.repository::<Ticket>();
//!     let mut t = tickets.find_by_id_for_update(&id).await?   // SELECT … FOR UPDATE
//!         .ok_or(OrmError::NotFound)?;
//!     t.status = Status::Paid;
//!     tickets.save(t).await?;
//!     Ok(total_cents)
//! })).await?;
//! ```

use crate::{entity::Entity, error::OrmError, BoxFuture};
use serde::{de::DeserializeOwned, Serialize};

/// Transaction-scoped CRUD for a single entity type.
///
/// Obtained from [`Transaction::repository`]. Every call runs on the
/// transaction's one connection, so reads and writes commit (or roll back)
/// together with the surrounding [`TransactionalDriver::transaction`] closure.
///
/// Calls within a transaction must be **sequential** (await each before the
/// next) — a single connection cannot multiplex, and that is exactly what a
/// transaction needs anyway.
pub trait TxRepository<T: Entity> {
    /// Find by primary key. `Ok(None)` for a simple miss.
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>>;

    /// Find by primary key, **locking the row** for the rest of the transaction
    /// (`SELECT … FOR UPDATE`).
    ///
    /// Any other transaction that tries to lock or update the same row blocks
    /// until this one commits or rolls back — the core primitive for a
    /// money-safe read-modify-write. On a backend whose dialect reports no lock
    /// clause this degrades to a plain read.
    fn find_by_id_for_update<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>>;

    /// Insert or update the entity, returning the stored form.
    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>>;

    /// Delete by primary key.
    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>>;
}

/// A live transaction handle, passed (owned) to the
/// [`TransactionalDriver::transaction`] closure. Vends transaction-scoped
/// repositories that share the transaction's connection.
pub trait Transaction {
    /// A repository bound to this transaction, for entity `T`.
    fn repository<T>(&self) -> Box<dyn TxRepository<T> + '_>
    where
        T: Entity + Serialize + DeserializeOwned + 'static;
}

/// A [`Driver`](crate::Driver) that can run an async closure inside a database
/// transaction.
///
/// Optional capability layered on `Driver`: only backends with real
/// transactions implement it. Implementors should also report
/// `capabilities().transactions == true`.
pub trait TransactionalDriver: crate::Driver {
    /// The backend's owned transaction handle (holds the connection; `'static`).
    type Tx: Transaction;

    /// Run `work` inside a single transaction.
    ///
    /// The driver checks out one connection, issues `BEGIN`, calls `work` with
    /// an owned [`Transaction`], then **commits if the returned future resolved
    /// to `Ok` and rolls back on `Err`**. The whole closure runs on one
    /// connection, so a row locked via [`TxRepository::find_by_id_for_update`]
    /// stays locked until the future resolves.
    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(Self::Tx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a;
}
