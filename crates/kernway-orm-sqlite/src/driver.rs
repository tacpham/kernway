use crate::SqliteRepository;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    BoxFuture,
};
use rusqlite::Connection;
use serde::{de::DeserializeOwned, Serialize};
use std::sync::{Arc, Mutex};

/// Shareable SQLite driver that vends repositories over one connection.
pub struct SqliteDriver {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteDriver {
    /// Open (or create) a SQLite database file.
    pub fn open(path: &str) -> Result<Self, OrmError> {
        let conn = Connection::open(path).map_err(|e| OrmError::Connection(e.to_string()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| OrmError::Connection(e.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open a private in-memory SQLite database.
    pub fn open_in_memory() -> Result<Self, OrmError> {
        let conn = Connection::open_in_memory().map_err(|e| OrmError::Connection(e.to_string()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| OrmError::Connection(e.to_string()))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

impl Driver for SqliteDriver {
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(SqliteRepository::<T>::from_conn(Arc::clone(&self.conn)))
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        Box::pin(async move {
            self.conn
                .lock()
                .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
                .execute_batch("SELECT 1")
                .map_err(|e| OrmError::Connection(e.to_string()))
        })
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            transactions: true,
            raw_query: true,
            full_text_search: false,
            json_columns: false,
            migrations: false,
        }
    }
}
