//! `MeilisearchRepository` — `Repository<T>` backed by Meilisearch.
//!
//! Uses the entity's `table_name()` as the Meilisearch index name and
//! the `#[id]` column name as the primary key field.
//!
//! All operations are **blocking HTTP** calls (via `ureq`) wrapped in
//! `rt_core::spawn_blocking`, the same pattern `kernway-orm-sqlite` uses.
//! The `meilisearch` feature flag must be enabled for the HTTP calls to
//! be compiled; without it every method returns `OrmError::Unsupported`.

use crate::{query::MeilisearchQueryBuilder, MeilisearchConfig};
use kernway_orm_core::{
    entity::Entity,
    error::OrmError,
    query::QueryBuilder,
    repository::Repository,
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;

/// Meilisearch repository for entity `T`.
///
/// The index name is `T::table_name()`.
/// The primary key field is the column with `primary_key = true` in `T::columns()`,
/// falling back to `"id"`.
pub struct MeilisearchRepository<T> {
    pub(crate) config: MeilisearchConfig,
    pub(crate) _marker: PhantomData<T>,
}

impl<T> MeilisearchRepository<T> {
    /// Create a repository from Meilisearch connection settings.
    pub fn new(config: MeilisearchConfig) -> Self {
        Self { config, _marker: PhantomData }
    }
}

/// Return the name of the primary-key field for entity `T`.
fn pk_field<T: Entity>() -> &'static str {
    T::columns()
        .iter()
        .find(|c| c.primary_key)
        .map(|c| c.name)
        .unwrap_or("id")
}

/// Serialize an ID value to a URL path segment.
/// Numbers → bare string; strings → string; anything else → JSON string.
fn id_to_string<Id: Serialize>(id: &Id) -> Result<String, OrmError> {
    let v = serde_json::to_value(id).map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    Ok(match v {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

// ── Repository<T> implementation ─────────────────────────────────────────────

impl<T> Repository<T> for MeilisearchRepository<T>
where
    T: Entity + Serialize + DeserializeOwned + Send + 'static,
{
    // ── Read ─────────────────────────────────────────────────────────────────

    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            let url_base = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            let id_str = match id_to_string(id) {
                Ok(s) => s,
                Err(e) => return Box::pin(async move { Err(e) }),
            };
            let index = T::table_name();
            Box::pin(async move {
                rt_core::spawn_blocking(move || -> Result<Option<T>, OrmError> {
                    let url = format!("{}/indexes/{}/documents/{}", url_base, index, id_str);
                    crate::api::get_optional(&url, &api_key)
                })
                .await
                .unwrap_or(Err(OrmError::Connection("blocking pool panicked".into())))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }

    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            let url_base = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            let index = T::table_name();
            Box::pin(async move {
                rt_core::spawn_blocking(move || -> Result<Vec<T>, OrmError> {
                    // Meilisearch max limit is 1000; do simple single-page fetch.
                    let url = format!("{}/indexes/{}/documents?limit=1000", url_base, index);
                    let result: crate::api::DocumentsResult<T> =
                        crate::api::get(&url, &api_key)?;
                    Ok(result.results)
                })
                .await
                .unwrap_or(Err(OrmError::Connection("blocking pool panicked".into())))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }

    fn find_all_by_ids<'a>(
        &'a self,
        ids: &'a [T::Id],
    ) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            let url_base = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            let index = T::table_name();
            let id_strings: Result<Vec<String>, OrmError> =
                ids.iter().map(id_to_string).collect();
            let id_strings = match id_strings {
                Ok(v) => v,
                Err(e) => return Box::pin(async move { Err(e) }),
            };
            Box::pin(async move {
                rt_core::spawn_blocking(move || -> Result<Vec<T>, OrmError> {
                    // Use search with an IN filter on the primary key field.
                    let pk = pk_field::<T>();
                    let list = id_strings
                        .iter()
                        .map(|s| {
                            if s.parse::<f64>().is_ok() {
                                s.clone()
                            } else {
                                format!("\"{}\"", s)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let filter = format!("{} IN [{}]", pk, list);
                    let req = crate::api::SearchRequest {
                        q: None,
                        filter: Some(filter),
                        sort: vec![],
                        limit: Some(id_strings.len() as u64),
                        offset: None,
                        hits_per_page: None,
                    };
                    let url = format!("{}/indexes/{}/search", url_base, index);
                    let result: crate::api::SearchResult<T> =
                        crate::api::post(&url, &api_key, &req)?;
                    Ok(result.hits)
                })
                .await
                .unwrap_or(Err(OrmError::Connection("blocking pool panicked".into())))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }

    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            let url_base = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            let index = T::table_name();
            Box::pin(async move {
                rt_core::spawn_blocking(move || -> Result<u64, OrmError> {
                    let url = format!("{}/indexes/{}/documents?limit=0", url_base, index);
                    let result: crate::api::DocumentsResult<serde_json::Value> =
                        crate::api::get(&url, &api_key)?;
                    Ok(result.total)
                })
                .await
                .unwrap_or(Err(OrmError::Connection("blocking pool panicked".into())))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }

    fn exists_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<bool, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            let url_base = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            let id_str = match id_to_string(id) {
                Ok(s) => s,
                Err(e) => return Box::pin(async move { Err(e) }),
            };
            let index = T::table_name();
            Box::pin(async move {
                rt_core::spawn_blocking(move || -> Result<bool, OrmError> {
                    let url = format!("{}/indexes/{}/documents/{}", url_base, index, id_str);
                    let exists: Option<serde_json::Value> =
                        crate::api::get_optional(&url, &api_key)?;
                    Ok(exists.is_some())
                })
                .await
                .unwrap_or(Err(OrmError::Connection("blocking pool panicked".into())))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }

    // ── Write ─────────────────────────────────────────────────────────────────

    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            let url_base = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            let index = T::table_name();
            let pk = pk_field::<T>();
            Box::pin(async move {
                rt_core::spawn_blocking(move || -> Result<T, OrmError> {
                    crate::api::ensure_index(&url_base, &api_key, index, pk)?;
                    let url = format!(
                        "{}/indexes/{}/documents?primaryKey={}",
                        url_base, index, pk
                    );
                    let task: crate::api::TaskEnqueued =
                        crate::api::post(&url, &api_key, &[&entity])?;
                    crate::api::wait_for_task(&url_base, &api_key, task.task_uid)?;
                    // Fetch back the saved document to return it (with any server-side changes).
                    let id_str = id_to_string(entity.id())?;
                    let saved_url =
                        format!("{}/indexes/{}/documents/{}", url_base, index, id_str);
                    crate::api::get_optional::<T>(&saved_url, &api_key)?
                        .ok_or(OrmError::NotFound)
                })
                .await
                .unwrap_or(Err(OrmError::Connection("blocking pool panicked".into())))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }

    fn save_all<'a>(&'a self, entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            let url_base = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            let index = T::table_name();
            let pk = pk_field::<T>();
            Box::pin(async move {
                rt_core::spawn_blocking(move || -> Result<Vec<T>, OrmError> {
                    crate::api::ensure_index(&url_base, &api_key, index, pk)?;
                    let url = format!(
                        "{}/indexes/{}/documents?primaryKey={}",
                        url_base, index, pk
                    );
                    let task: crate::api::TaskEnqueued =
                        crate::api::post(&url, &api_key, &entities)?;
                    crate::api::wait_for_task(&url_base, &api_key, task.task_uid)?;
                    // Fetch back the documents.
                    let id_strings: Result<Vec<String>, OrmError> =
                        entities.iter().map(|e| id_to_string(e.id())).collect();
                    let id_strings = id_strings?;
                    let list = id_strings
                        .iter()
                        .map(|s| {
                            if s.parse::<f64>().is_ok() { s.clone() }
                            else { format!("\"{}\"", s) }
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let filter = format!("{} IN [{}]", pk, list);
                    let req = crate::api::SearchRequest {
                        q: None,
                        filter: Some(filter),
                        sort: vec![],
                        limit: Some(entities.len() as u64),
                        offset: None,
                        hits_per_page: None,
                    };
                    let search_url = format!("{}/indexes/{}/search", url_base, index);
                    let result: crate::api::SearchResult<T> =
                        crate::api::post(&search_url, &api_key, &req)?;
                    Ok(result.hits)
                })
                .await
                .unwrap_or(Err(OrmError::Connection("blocking pool panicked".into())))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }

    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            let url_base = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            let id_str = match id_to_string(id) {
                Ok(s) => s,
                Err(e) => return Box::pin(async move { Err(e) }),
            };
            let index = T::table_name();
            Box::pin(async move {
                rt_core::spawn_blocking(move || -> Result<(), OrmError> {
                    let url =
                        format!("{}/indexes/{}/documents/{}", url_base, index, id_str);
                    // DELETE returns a task on 202; 404 means already gone.
                    match ureq::delete(&url)
                        .set("Authorization", &format!("Bearer {}", api_key))
                        .call()
                    {
                        Err(ureq::Error::Status(404, _)) => Err(OrmError::NotFound),
                        Err(e) => Err(crate::api::map_ureq(e)),
                        Ok(r) => {
                            let task: crate::api::TaskEnqueued = r
                                .into_json()
                                .map_err(|e| OrmError::TypeConversion(e.to_string()))?;
                            crate::api::wait_for_task(&url_base, &api_key, task.task_uid)
                        }
                    }
                })
                .await
                .unwrap_or(Err(OrmError::Connection("blocking pool panicked".into())))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }

    fn delete_all_by_ids<'a>(
        &'a self,
        ids: &'a [T::Id],
    ) -> BoxFuture<'a, Result<(), OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            let url_base = self.config.url.clone();
            let api_key = self.config.api_key.clone();
            let index = T::table_name();
            let id_strings: Result<Vec<String>, OrmError> =
                ids.iter().map(id_to_string).collect();
            let id_strings = match id_strings {
                Ok(v) => v,
                Err(e) => return Box::pin(async move { Err(e) }),
            };
            Box::pin(async move {
                rt_core::spawn_blocking(move || -> Result<(), OrmError> {
                    let url =
                        format!("{}/indexes/{}/documents/delete-batch", url_base, index);
                    let task: crate::api::TaskEnqueued =
                        crate::api::post(&url, &api_key, &id_strings)?;
                    crate::api::wait_for_task(&url_base, &api_key, task.task_uid)
                })
                .await
                .unwrap_or(Err(OrmError::Connection("blocking pool panicked".into())))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async { Err(OrmError::Unsupported("enable the `meilisearch` feature".into())) })
    }

    // ── Fluent query ──────────────────────────────────────────────────────────

    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(MeilisearchQueryBuilder::new(self.config.clone()))
    }
}
