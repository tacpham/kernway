//! PostgresDriver — implements [`kernway_orm_core::Driver`] for PostgreSQL.
//!
//! This is a **skeleton implementation**. It compiles and the API is stable,
//! but the actual tokio-postgres integration is incomplete — connection
//! pooling, row mapping, and transaction support need to be wired up.
//! Contributions welcome; see the `// TODO` comments for where to start.

use crate::PostgresRepository;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};

/// A configured handle to a PostgreSQL database.
pub struct PostgresDriver {
    url: String,
}

impl PostgresDriver {
    /// Create a new driver from a connection URL.
    ///
    /// ```rust,ignore
    /// let driver = PostgresDriver::connect("postgres://user:pass@localhost/mydb").await?;
    /// ```
    pub async fn connect(url: &str) -> Result<Self, OrmError> {
        Ok(Self {
            url: url.to_string(),
        })
    }
}

impl Driver for PostgresDriver {
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(PostgresRepository::<T>::new(self.url.clone()))
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        Box::pin(async move {
            Err(OrmError::Unsupported(
                "PostgresDriver::ping — not yet implemented".into(),
            ))
        })
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            transactions: true,
            raw_query: true,
            full_text_search: false,
            json_columns: true,
            migrations: false,
        }
    }
}
