//! MeilisearchDriver — implements [`kernway_orm_core::Driver`] for Meilisearch.

use crate::MeilisearchRepository;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};

/// Configuration for connecting to a Meilisearch instance.
#[derive(Clone)]
pub struct MeilisearchConfig {
    /// Meilisearch server URL, e.g. `"http://localhost:7700"`.
    pub url: String,
    /// API key (master key or search/admin key).
    pub api_key: String,
}

/// A configured handle to a Meilisearch instance.
pub struct MeilisearchDriver {
    config: MeilisearchConfig,
}

impl MeilisearchDriver {
    /// Create a new driver with the given config.
    pub fn new(config: MeilisearchConfig) -> Self {
        Self { config }
    }

    /// Convenience constructor.
    pub fn connect(url: &str, api_key: &str) -> Self {
        Self::new(MeilisearchConfig {
            url: url.to_string(),
            api_key: api_key.to_string(),
        })
    }
}

impl Driver for MeilisearchDriver {
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(MeilisearchRepository::<T>::new(self.config.clone()))
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        Box::pin(async move {
            Err(OrmError::Unsupported(
                "MeilisearchDriver::ping — not yet implemented".into(),
            ))
        })
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            transactions: false,
            raw_query: true,
            full_text_search: true,
            json_columns: true,
            migrations: false,
        }
    }
}
