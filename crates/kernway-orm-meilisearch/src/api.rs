//! Low-level Meilisearch REST API helpers — **async**, using `kernway-http-client`.
//!
//! All functions are `async fn` and run on Kernway's own runtime (no tokio,
//! no blocking thread pool). Authentication is `Authorization: Bearer <api_key>`.

use kernway_http_client::{HttpClient, Method, Request, Url};
use kernway_orm_core::error::OrmError;
use serde::{de::DeserializeOwned, Serialize};
use std::time::Duration;

// ── HTTP client ──────────────────────────────────────────────────────────────

/// Build a shared `HttpClient` for Meilisearch — 5 s connect, 30 s total.
pub fn client() -> HttpClient {
    HttpClient::new()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
}

fn bearer(api_key: &str) -> String {
    format!("Bearer {}", api_key)
}

// ── Error mapping ────────────────────────────────────────────────────────────

/// Convert an `HttpClient` response + status into an [`OrmError`].
fn map_status(status: u16, body: &[u8]) -> OrmError {
    let text = String::from_utf8_lossy(body).into_owned();
    match status {
        404 => OrmError::NotFound,
        409 => OrmError::UniqueViolation { field: text },
        _ => OrmError::Query(format!("Meilisearch HTTP {status}: {text}")),
    }
}

/// Convert an `HttpError` into an [`OrmError`].
fn map_http(e: kernway_http_client::HttpError) -> OrmError {
    OrmError::Connection(e.to_string())
}

// ── Generic async HTTP verbs ─────────────────────────────────────────────────

/// `GET {url}` and deserialise the JSON response body.
pub async fn get<T: DeserializeOwned>(url: &str, api_key: &str) -> Result<T, OrmError> {
    let resp = client()
        .send(
            Request::new(Method::Get, Url::parse(url).map_err(map_http)?)
                .header("Authorization", bearer(api_key)),
        )
        .await
        .map_err(map_http)?;

    if !resp.is_success() {
        return Err(map_status(resp.status, &resp.body));
    }
    serde_json::from_slice(&resp.body).map_err(|e| OrmError::TypeConversion(e.to_string()))
}

/// `GET {url}` — returns `None` on 404 instead of an error.
pub async fn get_optional<T: DeserializeOwned>(
    url: &str,
    api_key: &str,
) -> Result<Option<T>, OrmError> {
    let resp = client()
        .send(
            Request::new(Method::Get, Url::parse(url).map_err(map_http)?)
                .header("Authorization", bearer(api_key)),
        )
        .await
        .map_err(map_http)?;

    if resp.status == 404 {
        return Ok(None);
    }
    if !resp.is_success() {
        return Err(map_status(resp.status, &resp.body));
    }
    serde_json::from_slice(&resp.body)
        .map(Some)
        .map_err(|e| OrmError::TypeConversion(e.to_string()))
}

/// `POST {url}` with a JSON body, deserialise the JSON response body.
pub async fn post<Req: Serialize, Res: DeserializeOwned>(
    url: &str,
    api_key: &str,
    body: &Req,
) -> Result<Res, OrmError> {
    let json = serde_json::to_vec(body).map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    let resp = client()
        .send(
            Request::new(Method::Post, Url::parse(url).map_err(map_http)?)
                .header("Authorization", bearer(api_key))
                .body("application/json", json),
        )
        .await
        .map_err(map_http)?;

    if !resp.is_success() {
        return Err(map_status(resp.status, &resp.body));
    }
    serde_json::from_slice(&resp.body).map_err(|e| OrmError::TypeConversion(e.to_string()))
}

/// `DELETE {url}` — maps 404 to `OrmError::NotFound`.
pub async fn delete(url: &str, api_key: &str) -> Result<(), OrmError> {
    let resp = client()
        .send(
            Request::new(Method::Delete, Url::parse(url).map_err(map_http)?)
                .header("Authorization", bearer(api_key)),
        )
        .await
        .map_err(map_http)?;

    if resp.is_success() {
        Ok(())
    } else {
        Err(map_status(resp.status, &resp.body))
    }
}

// ── Meilisearch types ────────────────────────────────────────────────────────

/// Task enqueued by every write operation.
#[derive(Debug, serde::Deserialize)]
pub struct TaskEnqueued {
    /// The uid to poll on `GET /tasks/{uid}` until the write completes.
    #[serde(rename = "taskUid")]
    pub task_uid: u64,
}

/// Full task status from `GET /tasks/{uid}`.
#[derive(Debug, serde::Deserialize)]
pub struct TaskStatus {
    /// `"enqueued"`, `"processing"`, `"succeeded"`, or `"failed"`.
    pub status: String,
    /// Present when `status` is `"failed"`.
    pub error: Option<TaskError>,
}

/// Error detail inside a failed task.
#[derive(Debug, serde::Deserialize)]
pub struct TaskError {
    /// Human-readable failure message from Meilisearch.
    pub message: String,
}

/// Response from `GET /indexes/{uid}/documents`.
#[derive(Debug, serde::Deserialize)]
pub struct DocumentsResult<T> {
    /// The documents returned in this page.
    pub results: Vec<T>,
    /// Total documents in the index (independent of the page limit).
    pub total: u64,
}

/// Response from `POST /indexes/{uid}/search`.
#[derive(Debug, serde::Deserialize)]
pub struct SearchResult<T> {
    /// The matching documents.
    pub hits: Vec<T>,
    /// Approximate total match count (Meilisearch's default) — fast but not exact.
    #[serde(rename = "estimatedTotalHits")]
    pub estimated_total_hits: Option<u64>,
    /// Exact total match count, present only when the query requested it.
    #[serde(rename = "totalHits")]
    pub total_hits: Option<u64>,
}

/// `POST /indexes/{uid}/search` request body.
#[derive(Debug, Default, Serialize)]
pub struct SearchRequest<'a> {
    /// Full-text search terms (the `q` parameter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<&'a str>,
    /// Meilisearch filter expression, e.g. `role = "ADMIN" AND age > 18`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Sort expressions, e.g. `["price:asc"]`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<String>,
    /// Maximum hits to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Number of leading hits to skip (pagination).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

/// Response from `GET /health`.
#[derive(Debug, serde::Deserialize)]
pub struct HealthResponse {
    /// `"available"` when Meilisearch is ready to serve.
    pub status: String,
}

// ── Task polling ─────────────────────────────────────────────────────────────

/// Poll `GET /tasks/{task_uid}` until `succeeded` or `failed`.
///
/// Polls up to 100 times with 100 ms between each — max 10 s wait.
pub async fn wait_for_task(base_url: &str, api_key: &str, task_uid: u64) -> Result<(), OrmError> {
    let url = format!("{}/tasks/{}", base_url, task_uid);
    for _ in 0..100 {
        let task: TaskStatus = get(&url, api_key).await?;
        match task.status.as_str() {
            "succeeded" => return Ok(()),
            "failed" => {
                let msg = task
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "unknown error".into());
                return Err(OrmError::Query(format!("Meilisearch task failed: {msg}")));
            }
            _ => rt_core::sleep(Duration::from_millis(100)).await,
        }
    }
    Err(OrmError::Transaction(
        "Meilisearch task timed out after 10 s".into(),
    ))
}

// ── Index management ─────────────────────────────────────────────────────────

/// Create the index if it does not exist, setting the primary key.
///
/// Idempotent — safe to call before every write. We first check whether the
/// index exists (`GET /indexes/{uid}`) and only create it when absent: creating
/// an index that already exists returns a **202 whose task then fails** with
/// "already exists" in Meilisearch v1.7 (not a 400), so blindly enqueuing a
/// create is not safe. The task-failure case is tolerated too, as a guard
/// against a concurrent creator racing the existence check.
pub async fn ensure_index(
    base_url: &str,
    api_key: &str,
    index: &str,
    pk: &str,
) -> Result<(), OrmError> {
    // Already there? Nothing to do.
    let exists_url = format!("{}/indexes/{}", base_url, index);
    if get_optional::<serde_json::Value>(&exists_url, api_key)
        .await?
        .is_some()
    {
        return Ok(());
    }

    #[derive(Serialize)]
    struct Body<'a> {
        uid: &'a str,
        #[serde(rename = "primaryKey")]
        primary_key: &'a str,
    }

    let url = format!("{}/indexes", base_url);
    let json = serde_json::to_vec(&Body { uid: index, primary_key: pk })
        .map_err(|e| OrmError::TypeConversion(e.to_string()))?;

    let resp = client()
        .send(
            Request::new(Method::Post, Url::parse(&url).map_err(map_http)?)
                .header("Authorization", bearer(api_key))
                .body("application/json", json),
        )
        .await
        .map_err(map_http)?;

    match resp.status {
        // 202 = task enqueued to create the index → wait for it, but a concurrent
        // creator winning the race ("already exists") is success, not failure.
        202 => {
            let task: TaskEnqueued = serde_json::from_slice(&resp.body)
                .map_err(|e| OrmError::TypeConversion(e.to_string()))?;
            match wait_for_task(base_url, api_key, task.task_uid).await {
                Ok(()) => Ok(()),
                Err(OrmError::Query(m)) if m.contains("already exists") => Ok(()),
                Err(e) => Err(e),
            }
        }
        // 400 = index already exists in some Meilisearch versions — fine
        400 => Ok(()),
        s if (200..300).contains(&s) => Ok(()),
        _ => Err(map_status(resp.status, &resp.body)),
    }
}

// ── Health check ─────────────────────────────────────────────────────────────

/// `GET /health` — `Ok(())` when Meilisearch responds `"available"`.
pub async fn ping(base_url: &str, _api_key: &str) -> Result<(), OrmError> {
    let url = format!("{}/health", base_url);
    // Health endpoint needs no auth
    let resp = client()
        .send(Request::new(Method::Get, Url::parse(&url).map_err(map_http)?))
        .await
        .map_err(map_http)?;

    if !resp.is_success() {
        return Err(OrmError::Connection(format!(
            "Meilisearch health check failed: HTTP {}",
            resp.status
        )));
    }
    let h: HealthResponse = serde_json::from_slice(&resp.body)
        .map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    if h.status == "available" {
        Ok(())
    } else {
        Err(OrmError::Connection(format!(
            "Meilisearch not available: {}",
            h.status
        )))
    }
}

// ── Index settings ────────────────────────────────────────────────────────────

/// Set the filterable attributes for an index.
///
/// Fields listed here can be used in `filter` expressions (`filter_eq`, `filter_gt`, etc.).
/// Calling this triggers an index rebuild — waits for the task.
pub async fn set_filterable_attributes(
    base_url: &str,
    api_key: &str,
    index: &str,
    fields: &[&str],
) -> Result<(), OrmError> {
    let url = format!("{}/indexes/{}/settings/filterable-attributes", base_url, index);
    let json = serde_json::to_vec(fields).map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    let resp = client()
        .send(
            Request::new(Method::Put, Url::parse(&url).map_err(map_http)?)
                .header("Authorization", bearer(api_key))
                .body("application/json", json),
        )
        .await
        .map_err(map_http)?;
    let task: TaskEnqueued =
        serde_json::from_slice(&resp.body).map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    wait_for_task(base_url, api_key, task.task_uid).await
}

/// Set the sortable attributes for an index.
///
/// Fields listed here can be used in `order_by` / `order_by_desc`.
/// Triggers an index rebuild — waits for the task.
pub async fn set_sortable_attributes(
    base_url: &str,
    api_key: &str,
    index: &str,
    fields: &[&str],
) -> Result<(), OrmError> {
    let url = format!("{}/indexes/{}/settings/sortable-attributes", base_url, index);
    let json = serde_json::to_vec(fields).map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    let resp = client()
        .send(
            Request::new(Method::Put, Url::parse(&url).map_err(map_http)?)
                .header("Authorization", bearer(api_key))
                .body("application/json", json),
        )
        .await
        .map_err(map_http)?;
    let task: TaskEnqueued =
        serde_json::from_slice(&resp.body).map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    wait_for_task(base_url, api_key, task.task_uid).await
}

/// Update pagination settings — `max_total_hits` is the hard cap on total results
/// Meilisearch returns for a query (default 1000).
///
/// `PATCH /indexes/{uid}/settings` with `{ "pagination": { "maxTotalHits": N } }`
pub async fn set_pagination(
    base_url: &str,
    api_key: &str,
    index: &str,
    max_total_hits: u64,
) -> Result<(), OrmError> {
    #[derive(Serialize)]
    struct Pagination {
        #[serde(rename = "maxTotalHits")]
        max_total_hits: u64,
    }
    #[derive(Serialize)]
    struct Body {
        pagination: Pagination,
    }
    let url = format!("{}/indexes/{}/settings", base_url, index);
    let json = serde_json::to_vec(&Body { pagination: Pagination { max_total_hits } })
        .map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    let resp = client()
        .send(
            Request::new(Method::Patch, Url::parse(&url).map_err(map_http)?)
                .header("Authorization", bearer(api_key))
                .body("application/json", json),
        )
        .await
        .map_err(map_http)?;
    let task: TaskEnqueued =
        serde_json::from_slice(&resp.body).map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    wait_for_task(base_url, api_key, task.task_uid).await
}

/// Delete an index entirely (drops all documents and settings). Idempotent.
///
/// `DELETE /indexes/{uid}`
pub async fn drop_index(base_url: &str, api_key: &str, index: &str) -> Result<(), OrmError> {
    let url = format!("{}/indexes/{}", base_url, index);
    let resp = client()
        .send(
            Request::new(Method::Delete, Url::parse(&url).map_err(map_http)?)
                .header("Authorization", bearer(api_key)),
        )
        .await
        .map_err(map_http)?;
    match resp.status {
        202 => {
            let task: TaskEnqueued = serde_json::from_slice(&resp.body)
                .map_err(|e| OrmError::TypeConversion(e.to_string()))?;
            wait_for_task(base_url, api_key, task.task_uid).await
        }
        404 => Ok(()),
        _ => Err(map_status(resp.status, &resp.body)),
    }
}
