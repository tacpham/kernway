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
/// on demand. No persistent connection is kept — each call issues an async
/// request through `kernway-http-client` (Kernway's own runtime, no tokio, no
/// blocking pool).
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

/// Friendly alias for [`MeilisearchDriver`]. The type implements the ORM `Driver`
/// trait (hence the canonical `…Driver` name, matching `SqliteDriver` etc.), but
/// call sites read better as `Meilisearch::from_config(&cfg)` /
/// `Meilisearch::connect(url, key)`.
pub type Meilisearch = MeilisearchDriver;

impl MeilisearchDriver {
    /// Create a driver with the given [`MeilisearchConfig`].
    pub fn new(config: MeilisearchConfig) -> Self {
        Self { config }
    }

    /// Convenience constructor — takes URL and API key directly.
    pub fn connect(url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::new(MeilisearchConfig { url: url.into(), api_key: api_key.into() })
    }

    /// The connection settings this driver was built with.
    pub fn config(&self) -> &MeilisearchConfig {
        &self.config
    }

    /// Auto-configure from layered [`kernway_config::Config`] — Spring Boot starter
    /// style. Reads [`MeilisearchConfig`] via [`FromConfig`], so enabling the
    /// `config` feature and putting `meilisearch.host` + `meilisearch.api-key` in
    /// `application.yml` is all it takes to connect. (Requires the `config` feature.)
    #[cfg(feature = "config")]
    pub fn from_config(config: &kernway_config::Config) -> Self {
        use kernway_config::FromConfig;
        Self::new(MeilisearchConfig::from_config(config))
    }
}

/// Bind connection settings from layered config (`config` feature). Accepts the
/// Spring-compatible `meilisearch.host` (with `meilisearch.url` as an alias) and
/// `meilisearch.api-key` (alias `meilisearch.master-key`) — so the same
/// `application.yml`/`.properties` the Java app uses works unchanged.
#[cfg(feature = "config")]
impl kernway_config::FromConfig for MeilisearchConfig {
    fn from_config(config: &kernway_config::Config) -> Self {
        let url = config
            .get_str("meilisearch.host")
            .or_else(|| config.get_str("meilisearch.url"))
            .unwrap_or("http://localhost:7700")
            .to_string();
        let api_key = config
            .get_str("meilisearch.api-key")
            .or_else(|| config.get_str("meilisearch.master-key"))
            .unwrap_or_default()
            .to_string();
        Self { url, api_key }
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
