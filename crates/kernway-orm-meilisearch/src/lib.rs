//! # kernway-orm-meilisearch
//!
//! Meilisearch backend for `kernway-orm-core`.
//!
//! Meilisearch is a **search engine**, not a relational database, so this
//! driver does NOT use [`SqlDialect`]. It maps `Repository<T>` onto
//! Meilisearch's REST API using `kernway-http-client` — Kernway's own async
//! HTTP, on the same runtime as the server (no tokio, no blocking thread pool).
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
//! | `meilisearch` | Full async HTTP implementation via `kernway-http-client` |
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
//! | `save(e)` | `POST /documents` → wait task → `GET /documents/{id}` |
//! | `save_all(es)` | `POST /documents` (batch) → wait task → `GET /documents/{id}` each |
//! | `find_by_id(id)` | `GET /indexes/{index}/documents/{id}` |
//! | `find_all()` | `GET /documents?limit&offset`, paged over the whole index |
//! | `find_all_by_ids(ids)` | `GET /indexes/{index}/documents/{id}` per id |
//! | `count()` | `GET /documents?limit=0` → `total` (0 if the index is absent) |
//! | `exists_by_id(id)` | `GET /indexes/{index}/documents/{id}` → `Some` |
//! | `delete_by_id(id)` | `DELETE /indexes/{index}/documents/{id}` |
//! | `delete_all_by_ids(ids)` | `POST /indexes/{index}/documents/delete-batch` |
//! | `query().filter_*().fetch_*()` | `POST /indexes/{index}/search` |
//! | `ping()` | `GET /health` |
//!
//! ## Capability profile
//!
//! `kernway-orm-core` is a **spec with two tiers**: a small *basic* contract
//! (CRUD) that every driver must implement, plus a set of *extended* capabilities
//! (rich search, composite keys, relations, index tuning, …). Each backend, when
//! it implements the driver, evaluates which extensions it can support **natively**
//! and overrides those; where it cannot, the capability is simply absent — an
//! `OrmError::Unsupported`, a no-op, or a documented restriction — never faked.
//!
//! Meilisearch's profile (✅ native / override · ⚙️ conditional · ❌ not supported):
//!
//! | Capability | Meilisearch | Notes |
//! |---|---|---|
//! | Basic CRUD | ✅ | save / find / count / exists / delete + batch; `find_all` pages the whole index |
//! | Custom primary key | ✅ | any single `#[id]` field, string or numeric; the value must match `[A-Za-z0-9_-]` |
//! | Full-text search | ✅ **native strength** | `filter_like` → the `q` parameter: typo-tolerant and ranked (what Meilisearch is *for*) |
//! | Filtering (`eq`/`gt`/`in`/…) | ⚙️ | the field must be in `filterableAttributes` — see [`api::set_filterable_attributes`] |
//! | Sorting (`order_by`) | ⚙️ | the field must be in `sortableAttributes` — see [`api::set_sortable_attributes`] |
//! | Pagination | ✅ | `limit`/`offset`, `fetch_page`; totals are `estimatedTotalHits` (approximate), capped by `maxTotalHits` |
//! | Index tuning / attributes | ✅ **adds** | [`api::set_filterable_attributes`] / [`api::set_sortable_attributes`] / [`api::set_pagination`] — backend knobs beyond the generic ORM |
//! | Raw access | ✅ | the [`api`] module calls the REST endpoints directly — the escape hatch |
//! | Composite key | ❌ *(not yet)* | Meilisearch has a single PK field; a compound key needs a synthesized single-string id + an injected field |
//! | Relations (1-1 / 1-N / N-N) | ❌ | no joins; `query().with()` is a no-op — denormalize into the document instead |
//! | Transactions | ❌ | writes are async tasks; `capabilities().transactions == false` |
//! | Migrations | ❌ | schemaless; indexes are created on demand by `ensure_index` |
//!
//! The rule of thumb for a new driver: implement the basic tier, override the
//! extended capabilities the backend does well, and document the rest here rather
//! than emulating them poorly.
//!
//! [`SqlDialect`]: kernway_orm_core::dialect::SqlDialect
//! [`api`]: crate::api
//! [`api::set_filterable_attributes`]: crate::api::set_filterable_attributes
//! [`api::set_sortable_attributes`]: crate::api::set_sortable_attributes
//! [`api::set_pagination`]: crate::api::set_pagination

pub mod driver;
pub mod query;
pub mod repository;

pub use driver::{Meilisearch, MeilisearchConfig, MeilisearchDriver};
pub use repository::MeilisearchRepository;

#[cfg(feature = "meilisearch")]
pub mod api;
