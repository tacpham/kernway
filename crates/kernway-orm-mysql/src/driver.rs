//! MySqlDriver — implements [`kernway_orm_core::Driver`] for MySQL.
//!
//! This is a skeleton implementation intended to establish the extensible API.

use crate::MySqlRepository;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};

/// A configured handle to a MySQL database.
pub struct MySqlDriver {
    url: String,
}

impl MySqlDriver {
    /// Create a new driver from a connection URL.
    pub async fn connect(url: &str) -> Result<Self, OrmError> {
        Ok(Self {
            url: url.to_string(),
        })
    }
}

impl Driver for MySqlDriver {
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(MySqlRepository::<T>::new(self.url.clone()))
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        Box::pin(async move {
            Err(OrmError::Unsupported(
                "MySqlDriver::ping — not yet implemented".into(),
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
