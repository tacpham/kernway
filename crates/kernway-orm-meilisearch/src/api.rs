//! Low-level Meilisearch REST API helpers.
//!
//! All functions are **blocking** — call them from inside
//! `rt_core::spawn_blocking` closures, never directly from async code.
//!
//! Authentication is always `Authorization: Bearer <api_key>`.

use kernway_orm_core::error::OrmError;
use serde::{de::DeserializeOwned, Serialize};
use std::time::Duration;

// ── Error mapping ────────────────────────────────────────────────────────────

/// Convert a ureq error into an [`OrmError`].
pub fn map_ureq(e: ureq::Error) -> OrmError {
    match e {
        ureq::Error::Status(404, _) => OrmError::NotFound,
        ureq::Error::Status(409, r) => {
            let body = r.into_string().unwrap_or_default();
            OrmError::UniqueViolation { field: body }
        }
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            OrmError::Query(format!("Meilisearch HTTP {code}: {body}"))
        }
        ureq::Error::Transport(t) => OrmError::Connection(t.to_string()),
    }
}

fn bearer(api_key: &str) -> String {
    format!("Bearer {}", api_key)
}

// ── Generic HTTP verbs ───────────────────────────────────────────────────────

/// `GET {url}` and deserialise the JSON response body.
pub fn get<T: DeserializeOwned>(url: &str, api_key: &str) -> Result<T, OrmError> {
    ureq::get(url)
        .set("Authorization", &bearer(api_key))
        .call()
        .map_err(map_ureq)?
        .into_json::<T>()
        .map_err(|e| OrmError::TypeConversion(e.to_string()))
}

/// `GET {url}` and ignore a 404 (return `None` instead).
pub fn get_optional<T: DeserializeOwned>(url: &str, api_key: &str) -> Result<Option<T>, OrmError> {
    match ureq::get(url)
        .set("Authorization", &bearer(api_key))
        .call()
    {
        Ok(r) => r
            .into_json::<T>()
            .map(Some)
            .map_err(|e| OrmError::TypeConversion(e.to_string())),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(e) => Err(map_ureq(e)),
    }
}

/// `POST {url}` with a JSON body, deserialise the JSON response body.
pub fn post<Req: Serialize, Res: DeserializeOwned>(
    url: &str,
    api_key: &str,
    body: &Req,
) -> Result<Res, OrmError> {
    ureq::post(url)
        .set("Authorization", &bearer(api_key))
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(map_ureq)?
        .into_json::<Res>()
        .map_err(|e| OrmError::TypeConversion(e.to_string()))
}

/// `DELETE {url}` — maps 404 to `OrmError::NotFound`.
pub fn delete(url: &str, api_key: &str) -> Result<(), OrmError> {
    match ureq::delete(url)
        .set("Authorization", &bearer(api_key))
        .call()
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(404, _)) => Err(OrmError::NotFound),
        Err(e) => Err(map_ureq(e)),
    }
}

/// `DELETE {url}` — return `Ok(())` on 404 (already gone is fine for delete).
pub fn delete_idempotent(url: &str, api_key: &str) -> Result<(), OrmError> {
    match ureq::delete(url)
        .set("Authorization", &bearer(api_key))
        .call()
    {
        Ok(_) | Err(ureq::Error::Status(404, _)) => Ok(()),
        Err(e) => Err(map_ureq(e)),
    }
}

// ── Meilisearch types ────────────────────────────────────────────────────────

/// Task returned by every Meilisearch write operation.
#[derive(Debug, serde::Deserialize)]
pub struct TaskEnqueued {
    #[serde(rename = "taskUid")]
    pub task_uid: u64,
}

/// Full task status object returned by `GET /tasks/{uid}`.
#[derive(Debug, serde::Deserialize)]
pub struct TaskStatus {
    pub status: String,
    pub error: Option<TaskError>,
}

/// Error detail inside a failed task.
#[derive(Debug, serde::Deserialize)]
pub struct TaskError {
    pub message: String,
}

/// Response from `GET /indexes/{uid}/documents`.
#[derive(Debug, serde::Deserialize)]
pub struct DocumentsResult<T> {
    pub results: Vec<T>,
    pub total: u64,
}

/// Response from `POST /indexes/{uid}/search`.
#[derive(Debug, serde::Deserialize)]
pub struct SearchResult<T> {
    pub hits: Vec<T>,
    #[serde(rename = "estimatedTotalHits")]
    pub estimated_total_hits: Option<u64>,
    #[serde(rename = "totalHits")]
    pub total_hits: Option<u64>,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

/// `POST /indexes/{uid}/search` request body.
#[derive(Debug, Default, Serialize)]
pub struct SearchRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<String>,
    #[serde(rename = "hitsPerPage", skip_serializing_if = "Option::is_none")]
    pub hits_per_page: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
}

/// Response from `GET /health`.
#[derive(Debug, serde::Deserialize)]
pub struct HealthResponse {
    pub status: String,
}

// ── Task polling ─────────────────────────────────────────────────────────────

/// Poll `GET /tasks/{task_uid}` until `succeeded` or `failed`.
///
/// Retries up to 100 times with 100 ms sleep = max 10 s wait.
pub fn wait_for_task(base_url: &str, api_key: &str, task_uid: u64) -> Result<(), OrmError> {
    let url = format!("{}/tasks/{}", base_url, task_uid);
    for _ in 0..100 {
        let task: TaskStatus = get(&url, api_key)?;
        match task.status.as_str() {
            "succeeded" => return Ok(()),
            "failed" => {
                let msg = task
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "unknown error".into());
                return Err(OrmError::Query(format!("Meilisearch task failed: {msg}")));
            }
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    Err(OrmError::Transaction(
        "Meilisearch task timed out after 10 s".into(),
    ))
}

// ── Index management ─────────────────────────────────────────────────────────

/// Create the index if it does not exist, setting the primary key.
///
/// Idempotent — safe to call on every write. Meilisearch returns 202 on
/// creation and 200 if the index already exists.
pub fn ensure_index(base_url: &str, api_key: &str, index: &str, pk: &str) -> Result<(), OrmError> {
    #[derive(Serialize)]
    struct Body<'a> {
        uid: &'a str,
        #[serde(rename = "primaryKey")]
        primary_key: &'a str,
    }

    let url = format!("{}/indexes", base_url);
    // Meilisearch returns 202 (created) or 200 (already exists as a task for
    // the re-creation). If the index already exists with a different pk the
    // request is ignored — we just need the index to be there.
    match ureq::post(&url)
        .set("Authorization", &bearer(api_key))
        .set("Content-Type", "application/json")
        .send_json(&Body { uid: index, primary_key: pk })
    {
        Ok(r) => {
            // 202 Accepted — a task was enqueued to create the index
            if r.status() == 202 {
                let task: TaskEnqueued = r
                    .into_json()
                    .map_err(|e| OrmError::TypeConversion(e.to_string()))?;
                wait_for_task(base_url, api_key, task.task_uid)?;
            }
            Ok(())
        }
        // 400 can mean "index already exists" in older Meilisearch versions
        Err(ureq::Error::Status(400, _)) => Ok(()),
        Err(e) => Err(map_ureq(e)),
    }
}

// ── Health check ─────────────────────────────────────────────────────────────

/// `GET /health` — returns `Ok(())` when Meilisearch responds `"available"`.
pub fn ping(base_url: &str, _api_key: &str) -> Result<(), OrmError> {
    let url = format!("{}/health", base_url);
    let r: HealthResponse = ureq::get(&url)
        .call()
        .map_err(map_ureq)?
        .into_json()
        .map_err(|e| OrmError::TypeConversion(e.to_string()))?;
    if r.status == "available" {
        Ok(())
    } else {
        Err(OrmError::Connection(format!(
            "Meilisearch not available: {}",
            r.status
        )))
    }
}
