//! # kernway-orm-meilisearch
//!
//! Meilisearch backend for `kernway-orm-core`.
//!
//! Meilisearch is a **search engine**, not a relational database, so this
//! driver does NOT use [`SqlDialect`]. It maps `Repository<T>` onto
//! Meilisearch's REST API over blocking HTTP ([`ureq`]) wrapped in
//! [`rt_core::spawn_blocking`], following the same pattern as
//! `kernway-orm-sqlite`.
//!
//! ## Quick start
//!
//! ```toml
//! [dependencies]
//! kernway-orm-meilisearch = { version = "0.1", features = ["meilisearch"] }
//! ```
//!
//! ```rust,ignore
//! use kernway_orm_meilisearch::MeilisearchDriver;
//! use kernway_orm_core::{Driver, Repository};
//!
//! let driver = MeilisearchDriver::connect("http://localhost:7700", "masterKey");
//! driver.ping().await?;
//!
//! let products: Box<dyn Repository<Product>> = driver.repository();
//! let p = products.save(Product { id: 1, name: "Widget".into(), price: 9.99 }).await?;
//! let found = products.find_by_id(&1).await?;
//! ```
//!
//! ## Feature flags
//!
//! | Feature | Effect |
//! |---|---|
//! | *(none)* | Types compile; every operation returns `OrmError::Unsupported` |
//! | `meilisearch` | Full HTTP implementation via `ureq` + `rt_core::spawn_blocking` |
//!
//! Enable features selectively in your `Cargo.toml`:
//! ```toml
//! kernway-orm-meilisearch = { version = "0.1", features = ["meilisearch"] }
//! ```
//!
//! ## ORM → Meilisearch API mapping
//!
//! | ORM call | Meilisearch endpoint |
//! |---|---|
//! | `save(e)` | `POST /indexes/{index}/documents` |
//! | `save_all(es)` | `POST /indexes/{index}/documents` (batch) |
//! | `find_by_id(id)` | `GET /indexes/{index}/documents/{id}` |
//! | `find_all()` | `GET /indexes/{index}/documents?limit=1000` |
//! | `find_all_by_ids(ids)` | `POST /indexes/{index}/search` with IN filter |
//! | `count()` | `GET /indexes/{index}/documents?limit=0` → `total` |
//! | `exists_by_id(id)` | `GET /indexes/{index}/documents/{id}` → `Some` |
//! | `delete_by_id(id)` | `DELETE /indexes/{index}/documents/{id}` |
//! | `delete_all_by_ids(ids)` | `POST /indexes/{index}/documents/delete-batch` |
//! | `query().filter_*().fetch_*()` | `POST /indexes/{index}/search` |
//! | `ping()` | `GET /health` |
//!
//! ## Limitations vs SQL backends
//!
//! - No transactions (`capabilities().transactions == false`)
//! - `filter_like` maps to the full-text `q` query, not SQL LIKE
//! - Sorting requires the field to be in `sortableAttributes` index settings
//! - Filters require the field to be in `filterableAttributes` index settings
//! - `find_all()` fetches at most 1000 documents (Meilisearch limit per request)
//!
//! [`SqlDialect`]: kernway_orm_core::dialect::SqlDialect

pub mod driver;
pub mod query;
pub mod repository;

pub use driver::{MeilisearchConfig, MeilisearchDriver};
pub use repository::MeilisearchRepository;

#[cfg(feature = "meilisearch")]
pub(crate) mod api;
