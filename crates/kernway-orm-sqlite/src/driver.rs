//! `SqliteDriver` — the entry point: a [`SqlDb`] (pool, hand-written SQL,
//! repositories, transactions, cursors — all shared) over SQLite.

use crate::config::{Location, SqliteConfig};
use crate::conn::SqliteConn;
use crate::SqliteTx;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    transaction::TransactionalDriver,
    BoxFuture,
};
use crate::changeset::{Applied, ChangeFormat, Changeset, OnConflict};
use kernway_orm_sql::{run_transaction_on, SqlDb};
use kernway_wire::Connect;
use serde::{de::DeserializeOwned, Serialize};
use std::ops::Deref;
use std::path::Path;

/// Opens SQLite connections for the pool.
pub struct SqliteConnect(pub SqliteConfig);

impl Connect for SqliteConnect {
    type Conn = SqliteConn;
    type Error = OrmError;

    async fn connect(&self) -> Result<SqliteConn, OrmError> {
        SqliteConn::open(&self.0).await
    }
}

/// A SQLite database behind a per-core pool of connections, each on its own
/// thread.
///
/// Everything a [`SqlDb`] does is available directly (it derefs to one):
/// `execute`, `execute_with`, `query`, `query_as`, `query_scalar`,
/// `query_each`, `cursor`, `execute_many`, `execute_script`, `select`,
/// `repository`, `transaction`.
///
/// `!Send` by design, like every kernway driver: create one per shard (each
/// opens its own connections to the same file).
pub struct SqliteDriver {
    db: SqlDb<SqliteConnect>,
    /// Keeps an in-memory database alive while the pool's connections come
    /// and go.
    _anchor: Option<SqliteConn>,
}

impl SqliteDriver {
    /// Open the database file at `path` (created if missing, WAL mode).
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, OrmError> {
        Self::with_config(SqliteConfig::file(path.as_ref())).await
    }

    /// A fresh private in-memory database.
    pub async fn open_in_memory() -> Result<Self, OrmError> {
        Self::with_config(SqliteConfig::memory()).await
    }

    /// Open from a URL — see [`SqliteConfig::from_url`].
    pub async fn connect(url: &str) -> Result<Self, OrmError> {
        Self::with_config(SqliteConfig::from_url(url)?).await
    }

    /// Open with an explicit configuration. One connection is opened eagerly,
    /// so a missing file (read-only mode) or a bad pragma fails here.
    pub async fn with_config(cfg: SqliteConfig) -> Result<Self, OrmError> {
        let anchor = match cfg.location {
            Location::Memory(_) => Some(SqliteConn::open(&cfg).await?),
            Location::File(_) => None,
        };
        let size = cfg.pool_size;
        Ok(Self { db: SqlDb::open(SqliteConnect(cfg), size).await?, _anchor: anchor })
    }

    /// A new in-memory database holding a copy of `image` (bytes from
    /// [`serialize`](Self::serialize), or a database file's contents).
    pub async fn from_bytes(image: Vec<u8>) -> Result<Self, OrmError> {
        let mut cfg = SqliteConfig::memory();
        cfg.image = Some(std::sync::Arc::new(image));
        Self::with_config(cfg).await
    }

    /// The whole database as bytes — the same bytes a database file holds
    /// (open them with [`from_bytes`](Self::from_bytes), or write them to a file).
    pub async fn serialize(&self) -> Result<Vec<u8>, OrmError> {
        let mut g = self.db.pool().acquire().await?;
        g.run(|ctx| ctx.conn.serialize().map_err(crate::worker::map_err)).await
    }

    /// Size in bytes of the BLOB in `table.column` of row `rowid`.
    pub async fn blob_len(&self, table: &str, column: &str, rowid: i64) -> Result<usize, OrmError> {
        let (t, c) = (table.to_string(), column.to_string());
        let mut g = self.db.pool().acquire().await?;
        g.run(move |ctx| Ok(ctx.blob(&t, &c, rowid, true)?.len())).await
    }

    /// Read `len` bytes at `offset` of a BLOB without loading the rest
    /// (incremental BLOB I/O). Reading past the end is an error.
    pub async fn blob_read(&self, table: &str, column: &str, rowid: i64, offset: usize, len: usize) -> Result<Vec<u8>, OrmError> {
        let (t, c) = (table.to_string(), column.to_string());
        let mut g = self.db.pool().acquire().await?;
        g.run(move |ctx| {
            ctx.blob(&t, &c, rowid, true)?.read(offset, len).map_err(crate::worker::map_err)
        })
        .await
    }

    /// Overwrite bytes at `offset` of a BLOB in place. A BLOB never grows
    /// this way: create it at full size first (`zeroblob(n)`), then fill it
    /// piece by piece.
    pub async fn blob_write(&self, table: &str, column: &str, rowid: i64, offset: usize, data: &[u8]) -> Result<(), OrmError> {
        let (t, c, data) = (table.to_string(), column.to_string(), data.to_vec());
        let mut g = self.db.pool().acquire().await?;
        g.run(move |ctx| {
            ctx.blob(&t, &c, rowid, false)?.write(offset, &data).map_err(crate::worker::map_err)
        })
        .await
    }

    /// Run `work` in a transaction and record what it changed in `tables`
    /// (every table when empty) as a [`Changeset`] — returned with `work`'s
    /// result once the transaction committed. A failed transaction records
    /// nothing. Tables need a declared `PRIMARY KEY`.
    ///
    /// ```rust,ignore
    /// let (_, changes) = db.track_changes(&["ticket"], |tx| Box::pin(async move {
    ///     tx.execute_with("UPDATE ticket SET status = 'paid' WHERE id = ?", params![7]).await
    /// })).await?;
    /// cloud.apply_changeset(&changes, OnConflict::Abort).await?;   // elsewhere
    /// ```
    pub async fn track_changes<'a, R, F>(&'a self, tables: &[&str], work: F) -> Result<(R, Changeset), OrmError>
    where
        F: FnOnce(SqliteTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        self.capture(tables, ChangeFormat::Changeset, work).await
    }

    /// [`track_changes`](Self::track_changes), recording a smaller patchset
    /// (keys and new values only; cannot be inverted).
    pub async fn track_patch<'a, R, F>(&'a self, tables: &[&str], work: F) -> Result<(R, Changeset), OrmError>
    where
        F: FnOnce(SqliteTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        self.capture(tables, ChangeFormat::Patchset, work).await
    }

    async fn capture<'a, R, F>(&'a self, tables: &[&str], format: ChangeFormat, work: F) -> Result<(R, Changeset), OrmError>
    where
        F: FnOnce(SqliteTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        let tables: Vec<String> = tables.iter().map(|t| t.to_string()).collect();
        let mut g = self.db.pool().acquire().await?;
        let id = g.run(move |ctx| ctx.start_session(&tables)).await?;
        let (g, result) = run_transaction_on(g, work).await;
        let mut g = g.ok_or_else(|| OrmError::Transaction("connection lost during transaction".into()))?;
        match result {
            Ok(value) => {
                let bytes = g.run(move |ctx| ctx.finish_session(id, format)).await?;
                Ok((value, Changeset::from_bytes(bytes, format)))
            }
            Err(e) => {
                let _ = g
                    .run(move |ctx| {
                        ctx.sessions.remove(&id);
                        Ok(())
                    })
                    .await;
                Err(e)
            }
        }
    }

    /// Apply a changeset (from [`track_changes`](Self::track_changes), here
    /// or on another database with the same tables) in one savepoint,
    /// resolving conflicts per `on_conflict`.
    pub async fn apply_changeset(&self, changes: &Changeset, on_conflict: OnConflict) -> Result<Applied, OrmError> {
        let bytes = changes.as_bytes().to_vec();
        let mut g = self.db.pool().acquire().await?;
        let conflicts = g.run(move |ctx| ctx.apply(&bytes, on_conflict)).await?;
        Ok(Applied { conflicts })
    }

    /// Copy the database into a new file (SQLite's online backup — a
    /// consistent snapshot while the database stays in use). Works for an
    /// in-memory database too.
    pub async fn backup_to(&self, path: impl AsRef<Path>) -> Result<(), OrmError> {
        let mut g = self.db.pool().acquire().await?;
        g.backup_to(path.as_ref()).await
    }
}

impl Deref for SqliteDriver {
    type Target = SqlDb<SqliteConnect>;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl Driver for SqliteDriver {
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        self.db.repository()
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        self.db.ping()
    }

    fn capabilities(&self) -> DriverCapabilities {
        Driver::capabilities(&self.db)
    }
}

impl TransactionalDriver for SqliteDriver {
    type Tx = SqliteTx;

    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(SqliteTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        self.db.transaction(work)
    }
}
