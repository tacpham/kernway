//! `MeilisearchDriver` — implements [`Driver`] for Meilisearch.

use crate::repository::MeilisearchRepository;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};

/// Connection settings for a Meilisearch instance.
#[derive(Clone, Debug)]
pub struct MeilisearchConfig {
    /// Meilisearch server URL, e.g. `"http://localhost:7700"`.
    pub url: String,
    /// API key (master key or a scoped search/admin key).
    pub api_key: String,
}

/// A configured handle to a Meilisearch instance.
///
/// Holds the connection config and creates [`MeilisearchRepository`] instances
/// on demand. No persistent connection is kept — each call opens a new
/// `ureq` request on a blocking pool thread.
///
/// # Example
/// ```rust,ignore
/// let driver = MeilisearchDriver::connect("http://localhost:7700", "masterKey");
/// driver.ping().await?;
/// let products: Box<dyn Repository<Product>> = driver.repository();
/// ```
pub struct MeilisearchDriver {
    pub(crate) config: MeilisearchConfig,
}

impl MeilisearchDriver {
    /// Create a driver with the given [`MeilisearchConfig`].
    pub fn new(config: MeilisearchConfig) -> Self {
        Self { config }
    }

    /// Convenience constructor — takes URL and API key directly.
    pub fn connect(url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::new(MeilisearchConfig { url: url.into(), api_key: api_key.into() })
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
        #[cfg(feature = "meilisearch")]
        {
            let url = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            Box::pin(async move { crate::api::ping(&url, &api_key).await })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async {
            Err(OrmError::Unsupported(
                "enable the `meilisearch` feature to use MeilisearchDriver".into(),
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
