//! # kernway-orm-meilisearch
//!
//! Meilisearch backend for `kernway-orm-core`.
//!
//! Meilisearch is a **search engine**, not a relational database, so this
//! driver does NOT use [`SqlDialect`]. It maps `Repository<T>` onto
//! Meilisearch's REST API:
//!
//! | ORM operation | Meilisearch API call |
//! |---|---|
//! | `save(entity)` | `POST /indexes/{index}/documents` |
//! | `find_by_id(id)` | `GET /indexes/{index}/documents/{id}` |
//! | `delete_by_id(id)` | `DELETE /indexes/{index}/documents/{id}` |
//! | `query().filter_eq("field", "value")` | filter expression in search request |
//! | `query().fetch_all()` | `POST /indexes/{index}/search` |
//!
//! ## Limitations vs SQL backends
//! - No transactions (`capabilities().transactions == false`)
//! - `filter_like` maps to full-text search, not SQL LIKE
//! - Ordering requires the field to be sortable in Meilisearch settings
//! - `filter_between` requires the field to be filterable in Meilisearch settings
//!
//! [`SqlDialect`]: kernway_orm_core::dialect::SqlDialect

pub mod driver;
pub use driver::MeilisearchDriver;

pub mod repository;
pub use repository::MeilisearchRepository;
