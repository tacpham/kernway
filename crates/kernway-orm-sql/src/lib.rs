//! The SQL half of the kernway ORM, written once for every relational backend.
//!
//! A database driver implements one small trait, [`SqlConn`] — run a query and
//! decode its rows, execute a statement, insert and return the generated key,
//! and transaction control — plus its [`SqlDialect`](kernway_orm_core::SqlDialect).
//! Everything above that is shared:
//!
//! - [`engine`]: CRUD SQL generation and execution (find, save, batch insert,
//!   delete, by-id lists, row locks).
//! - [`SqlQueryBuilder`]: filters, [`Spec`](kernway_orm_core::spec::Spec)s,
//!   ordering, pagination — every value is a bound parameter, never inlined.
//! - [`SqlRepository`] and [`run_transaction`] / [`SqlTx`]: the pooled
//!   autocommit repository and the transaction scope.
//!
//! Values cross into the backend as neutral [`Param`]s and come back as
//! entities through the shared row decoder (`kernway_orm_core::decode`), so
//! every backend maps entity fields to SQL values the same way.

pub mod engine;
mod param;
mod query;
mod repository;
mod transaction;

pub use param::{column_for_field, filter_param, id_to_params, is_auto_pk, is_new_auto_entity, json_to_param, pk_cols, Param};
pub use query::SqlQueryBuilder;
pub use repository::SqlRepository;
pub use transaction::{run_transaction, SqlTx};

use kernway_orm_core::{entity::Entity, error::OrmError, SqlDialect};
use serde::de::DeserializeOwned;
use std::future::Future;

/// One database connection, as the shared ORM layer needs it. Everything
/// database-specific about running SQL lives behind these methods.
pub trait SqlConn {
    /// The SQL dialect of this backend.
    fn dialect() -> &'static dyn SqlDialect
    where
        Self: Sized;

    /// Run a query and deserialize each row into `T` (columns in
    /// `T::columns()` order).
    fn select<T: Entity + DeserializeOwned>(
        &mut self,
        sql: &str,
        params: &[Param],
    ) -> impl Future<Output = Result<Vec<T>, OrmError>>;

    /// Run a query whose first column of the first row is a count.
    fn select_count(&mut self, sql: &str, params: &[Param]) -> impl Future<Output = Result<u64, OrmError>>;

    /// Execute a statement; returns the number of rows affected.
    fn execute(&mut self, sql: &str, params: &[Param]) -> impl Future<Output = Result<u64, OrmError>>;

    /// Execute an `INSERT` into a table with a generated key `pk_col` and
    /// return the new key.
    fn insert_returning_id(
        &mut self,
        insert_sql: &str,
        params: &[Param],
        pk_col: &str,
    ) -> impl Future<Output = Result<Param, OrmError>>;

    /// Execute a multi-row `INSERT` of `rows` rows with generated keys and
    /// return the first new key, when the backend guarantees the keys are
    /// consecutive (MySQL). `Ok(None)` (the default) makes the engine insert
    /// row by row instead.
    fn insert_batch_first_id(
        &mut self,
        _insert_sql: &str,
        _params: &[Param],
        _rows: usize,
    ) -> impl Future<Output = Result<Option<u64>, OrmError>> {
        async { Ok(None) }
    }

    /// Start a transaction: until [`commit`](Self::commit) or
    /// [`rollback`](Self::rollback), statements are not committed on their own.
    fn begin(&mut self) -> impl Future<Output = Result<(), OrmError>>;

    /// Commit the transaction.
    fn commit(&mut self) -> impl Future<Output = Result<(), OrmError>>;

    /// Roll the transaction back.
    fn rollback(&mut self) -> impl Future<Output = Result<(), OrmError>>;
}

/// A pool connector whose connections speak [`SqlConn`].
pub trait SqlConnect: kernway_wire::Connect<Conn = Self::Sql, Error = OrmError> + 'static {
    /// The connection type (same as `Connect::Conn`).
    type Sql: SqlConn + 'static;
}

impl<C> SqlConnect for C
where
    C: kernway_wire::Connect<Error = OrmError> + 'static,
    C::Conn: SqlConn + 'static,
{
    type Sql = C::Conn;
}
