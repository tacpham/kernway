//! `MeilisearchRepository` — `Repository<T>` backed by Meilisearch.
//!
//! Uses `T::table_name()` as the index name and the `#[id]` column as the
//! primary key. All operations are **truly async** via `kernway-http-client`
//! (Kernway's own async HTTP, no tokio, no blocking thread pool).
//!
//! Two `impl` blocks gate the feature:
//! - Without `meilisearch` feature: every method returns `OrmError::Unsupported`.
//! - With `meilisearch` feature: full HTTP implementation.

use crate::{query::MeilisearchQueryBuilder, MeilisearchConfig};
use kernway_orm_core::{entity::Entity, error::OrmError, query::QueryBuilder, repository::Repository, BoxFuture};
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;

/// Meilisearch repository for entity `T`.
pub struct MeilisearchRepository<T> {
    pub(crate) config: MeilisearchConfig,
    pub(crate) _marker: PhantomData<T>,
}

impl<T> MeilisearchRepository<T> {
    /// Create a repository from connection settings.
    pub fn new(config: MeilisearchConfig) -> Self {
        Self { config, _marker: PhantomData }
    }
}

// ── Stub impl (no feature) ────────────────────────────────────────────────────
// Every method returns Unsupported. Compiled when the `meilisearch` feature is OFF.

#[cfg(not(feature = "meilisearch"))]
impl<T> Repository<T> for MeilisearchRepository<T>
where
    T: Entity + Serialize + DeserializeOwned + Send + 'static,
{
    fn find_by_id<'a>(&'a self, _id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }
    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }
    // find_all_by_ids and exists_by_id use the trait defaults (built on
    // find_by_id, which returns Unsupported here).
    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }
    fn save<'a>(&'a self, _entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }
    fn save_all<'a>(&'a self, _entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }
    fn delete_by_id<'a>(&'a self, _id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }
    fn delete_all_by_ids<'a>(&'a self, _ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }
    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(MeilisearchQueryBuilder::new(self.config.clone()))
    }
}

// ── Full async impl (with feature) ────────────────────────────────────────────
// Compiled only when the `meilisearch` feature is ON.
// Uses kernway-http-client for all HTTP calls — no blocking thread pool.

#[cfg(feature = "meilisearch")]
fn pk_field<T: Entity>() -> &'static str {
    T::columns().iter().find(|c| c.primary_key).map(|c| c.name).unwrap_or("id")
}

#[cfg(feature = "meilisearch")]
fn id_to_string<Id: Serialize>(id: &Id) -> Result<String, OrmError> {
    let v = serde_json::to_value(id).map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    Ok(match v {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

#[cfg(feature = "meilisearch")]
impl<T> Repository<T> for MeilisearchRepository<T>
where
    T: Entity + Serialize + DeserializeOwned + Send + 'static,
{
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        let id_str = match id_to_string(id) {
            Ok(s) => s,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        Box::pin(async move {
            let url = format!("{}/indexes/{}/documents/{}", self.config.url, T::table_name(), id_str);
            crate::api::get_optional(&url, &self.config.api_key).await
        })
    }

    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async move {
            // Page through the whole index rather than silently capping at the
            // first N — `find_all` means all. The GET /documents endpoint is not
            // subject to the search `maxTotalHits` limit, so offset pagination
            // reaches every document.
            const PAGE: u64 = 1000;
            let index = T::table_name();
            let mut all: Vec<T> = Vec::new();
            let mut offset: u64 = 0;
            loop {
                let url = format!(
                    "{}/indexes/{}/documents?limit={}&offset={}",
                    self.config.url, index, PAGE, offset
                );
                let r: crate::api::DocumentsResult<T> =
                    match crate::api::get(&url, &self.config.api_key).await {
                        Ok(r) => r,
                        // Index not created yet → treat as empty, not an error.
                        Err(OrmError::NotFound) => break,
                        Err(e) => return Err(e),
                    };
                let got = r.results.len() as u64;
                all.extend(r.results);
                offset += got;
                // Stop when the last page was short or we've collected the total.
                if got < PAGE || offset >= r.total {
                    break;
                }
            }
            Ok(all)
        })
    }

    // find_all_by_ids uses the trait default: one find_by_id (GET /documents/{id})
    // per id. Meilisearch has no batch get-by-id, and searching by a PK `IN`
    // filter would need the PK to be `filterable`, so the default is the best path.

    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        Box::pin(async move {
            let url = format!("{}/indexes/{}/documents?limit=0", self.config.url, T::table_name());
            match crate::api::get::<crate::api::DocumentsResult<serde_json::Value>>(
                &url,
                &self.config.api_key,
            )
            .await
            {
                Ok(r) => Ok(r.total),
                // Index not created yet → count is 0, not an error.
                Err(OrmError::NotFound) => Ok(0),
                Err(e) => Err(e),
            }
        })
    }

    // exists_by_id uses the trait default: find_by_id(id).is_some(), which is
    // exactly the GET /documents/{id} probe we would write by hand.

    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        Box::pin(async move {
            let pk = pk_field::<T>();
            let index = T::table_name();
            crate::api::ensure_index(&self.config.url, &self.config.api_key, index, pk).await?;
            let url = format!("{}/indexes/{}/documents?primaryKey={}", self.config.url, index, pk);
            let task: crate::api::TaskEnqueued =
                crate::api::post(&url, &self.config.api_key, &[&entity]).await?;
            crate::api::wait_for_task(&self.config.url, &self.config.api_key, task.task_uid).await?;
            let id_str = id_to_string(entity.id())?;
            let get_url = format!("{}/indexes/{}/documents/{}", self.config.url, index, id_str);
            crate::api::get_optional::<T>(&get_url, &self.config.api_key).await?.ok_or(OrmError::NotFound)
        })
    }

    fn save_all<'a>(&'a self, entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async move {
            let pk = pk_field::<T>();
            let index = T::table_name();
            crate::api::ensure_index(&self.config.url, &self.config.api_key, index, pk).await?;
            let url = format!("{}/indexes/{}/documents?primaryKey={}", self.config.url, index, pk);
            let task: crate::api::TaskEnqueued =
                crate::api::post(&url, &self.config.api_key, &entities).await?;
            crate::api::wait_for_task(&self.config.url, &self.config.api_key, task.task_uid).await?;
            // Fetch each saved document back by id via GET (no filterable-PK
            // requirement, and preserves input order).
            let id_strings: Result<Vec<String>, OrmError> =
                entities.iter().map(|e| id_to_string(e.id())).collect();
            let id_strings = id_strings?;
            let mut saved = Vec::with_capacity(id_strings.len());
            for id in &id_strings {
                let get_url = format!("{}/indexes/{}/documents/{}", self.config.url, index, id);
                if let Some(doc) =
                    crate::api::get_optional::<T>(&get_url, &self.config.api_key).await?
                {
                    saved.push(doc);
                }
            }
            Ok(saved)
        })
    }

    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        let id_str = match id_to_string(id) {
            Ok(s) => s,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        Box::pin(async move {
            use kernway_http_client::{Method, Request, Url};
            let url = format!("{}/indexes/{}/documents/{}", self.config.url, T::table_name(), id_str);
            let resp = crate::api::client()
                .send(
                    Request::new(Method::Delete, Url::parse(&url).map_err(|e| OrmError::Connection(e.to_string()))?)
                        .header("Authorization", format!("Bearer {}", self.config.api_key)),
                )
                .await
                .map_err(|e| OrmError::Connection(e.to_string()))?;
            if resp.status == 404 { return Err(OrmError::NotFound); }
            if !resp.is_success() {
                return Err(OrmError::Query(format!("Meilisearch HTTP {}: {}",
                    resp.status, String::from_utf8_lossy(&resp.body))));
            }
            let task: crate::api::TaskEnqueued = serde_json::from_slice(&resp.body)
                .map_err(|e| OrmError::TypeConversion(e.to_string()))?;
            crate::api::wait_for_task(&self.config.url, &self.config.api_key, task.task_uid).await
        })
    }

    fn delete_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        let id_strings: Result<Vec<String>, OrmError> = ids.iter().map(id_to_string).collect();
        let id_strings = match id_strings {
            Ok(v) => v,
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        Box::pin(async move {
            let url = format!("{}/indexes/{}/documents/delete-batch", self.config.url, T::table_name());
            let task: crate::api::TaskEnqueued =
                crate::api::post(&url, &self.config.api_key, &id_strings).await?;
            crate::api::wait_for_task(&self.config.url, &self.config.api_key, task.task_uid).await
        })
    }

    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(MeilisearchQueryBuilder::new(self.config.clone()))
    }
}
