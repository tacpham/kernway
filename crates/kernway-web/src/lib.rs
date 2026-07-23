//! kernway-web — HTTP extractors + response types.

#![forbid(unsafe_code)]

use kernway_core::{
    error::StatusCode,
    request::Request,
    response::{IntoResponse, Response},
};
use serde::{de::DeserializeOwned, Serialize};

// ============================================================
// Json<T> — request extractor + response type
// ============================================================

/// JSON response wrapper.
///
/// `-> Json<User>` → Content-Type: application/json
///
/// ```rust,ignore
/// #[route(GET, "/users/{id}")]
/// fn get_user(req: &Request, ctx: &AppContext) -> Json<User> {
///     Json(User { id: 1, name: "Alice".into() })
/// }
/// ```
pub struct Json<T>(pub T);

impl<T: Serialize + Send> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        match serde_json::to_vec(&self.0) {
            Ok(body) => Response::new(StatusCode::OK)
                .content_type("application/json; charset=utf-8")
                .body(body),
            Err(e) => Response::new(StatusCode::INTERNAL_SERVER_ERROR)
                .content_type("application/json; charset=utf-8")
                .body(format!(r#"{{"error":"serialize error: {}"}}"#, e).into_bytes()),
        }
    }
}

impl<T: DeserializeOwned> Json<T> {
    /// Extract a JSON body from the request.
    pub fn from_request(req: &Request) -> Result<Self, String> {
        serde_json::from_slice(&req.body)
            .map(Json)
            .map_err(|e| format!("invalid JSON body: {}", e))
    }
}

// ============================================================
// Path<T> — path parameter extractor
// ============================================================

/// Path parameter extractor.
///
/// ```rust,ignore
/// fn handler(req: &Request) -> Json<User> {
///     let id = Path::<u64>::from_request(req, "id").unwrap();
///     Json(service.find(*id))
/// }
/// ```
pub struct Path<T>(pub T);

impl<T: std::str::FromStr> Path<T>
where
    T::Err: std::fmt::Display,
{
    /// Pull `param` out of the matched route pattern and parse it into `T`.
    ///
    /// Fails when the router captured no such placeholder, or when the captured
    /// text does not parse — both are client errors, so map them to a 400.
    pub fn from_request(req: &Request, param: &str) -> Result<Self, String> {
        let val = req
            .path_params
            .get(param)
            .ok_or_else(|| format!("path param `{}` not found", param))?;
        val.parse::<T>()
            .map(Path)
            .map_err(|e| format!("invalid path param `{}`: {}", param, e))
    }
}

impl<T> std::ops::Deref for Path<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.0 }
}

// ============================================================
// Query<T> — query string extractor
// ============================================================

/// Query string extractor.
pub struct Query<T>(pub T);

impl<T: DeserializeOwned> Query<T> {
    /// Deserialize the query string into `T`.
    ///
    /// Every value arrives as a string, so `T`'s non-string fields need
    /// `#[serde(deserialize_with = ...)]` or a string-tolerant type.
    pub fn from_request(req: &Request) -> Result<Self, String> {
        // Built as a `serde_json::Map` rather than serialized to a string and
        // parsed back: the round trip through text was only ever there to reach
        // `from_str`. `QueryParams` deliberately does not implement `Serialize`
        // — `kernway-core` is spec-only and carries no serde dependency.
        let map: serde_json::Map<String, serde_json::Value> = req
            .query
            .iter()
            .map(|(name, value)| (name.to_string(), serde_json::Value::String(value.to_string())))
            .collect();
        serde_json::from_value(serde_json::Value::Object(map))
            .map(Query)
            .map_err(|e| format!("invalid query params: {}", e))
    }
}

// ============================================================
// Error response helpers
// ============================================================

/// RFC 7807 Problem Details error response.
#[derive(Serialize)]
pub struct ProblemDetail {
    /// HTTP status code, repeated in the body as RFC 7807 §3.1 allows.
    pub status:  u16,
    /// Short, human-readable summary — stable for a given status.
    pub title:   &'static str,
    /// Explanation specific to this occurrence.
    pub detail:  String,
}

impl ProblemDetail {
    /// Build a `404 Not Found` problem response.
    pub fn not_found(detail: impl Into<String>) -> Response {
        let mut r = Json(ProblemDetail {
            status: 404,
            title:  "Not Found",
            detail: detail.into(),
        })
        .into_response();
        r.status = StatusCode::NOT_FOUND;
        r
    }

    /// Build a `400 Bad Request` problem response.
    pub fn bad_request(detail: impl Into<String>) -> Response {
        let mut r = Json(ProblemDetail {
            status: 400,
            title:  "Bad Request",
            detail: detail.into(),
        })
        .into_response();
        r.status = StatusCode::BAD_REQUEST;
        r
    }

    /// Build a `500 Internal Server Error` problem response.
    ///
    /// Keep `detail` free of internal specifics — it reaches the client.
    pub fn internal_error(detail: impl Into<String>) -> Response {
        let mut r = Json(ProblemDetail {
            status: 500,
            title:  "Internal Server Error",
            detail: detail.into(),
        })
        .into_response();
        r.status = StatusCode::INTERNAL_SERVER_ERROR;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernway_core::request::Request;
    use serde::Deserialize;

    // --- Json<T> ---

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct User { id: u64, name: String }

    #[test]
    fn json_into_response_sets_content_type() {
        let resp = Json(User { id: 1, name: "Alice".into() }).into_response();
        assert_eq!(resp.status.0, 200);
        assert_eq!(
            resp.headers.get("content-type").unwrap(),
            "application/json; charset=utf-8"
        );
    }

    #[test]
    fn json_into_response_body_is_valid_json() {
        let resp = Json(User { id: 2, name: "Bob".into() }).into_response();
        let parsed: User = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(parsed, User { id: 2, name: "Bob".into() });
    }

    #[test]
    fn json_from_request_valid_body() {
        let mut req = Request::new("POST", "/users");
        req.body = br#"{"id":3,"name":"Charlie"}"#.to_vec();
        let Json(user) = Json::<User>::from_request(&req).unwrap();
        assert_eq!(user.id, 3);
        assert_eq!(user.name, "Charlie");
    }

    #[test]
    fn json_from_request_invalid_body_returns_error() {
        let mut req = Request::new("POST", "/users");
        req.body = b"not json".to_vec();
        assert!(Json::<User>::from_request(&req).is_err());
    }

    // --- Path<T> ---

    #[test]
    fn path_extracts_u64() {
        let mut req = Request::new("GET", "/users/42");
        req.path_params.insert("id".to_string(), "42".to_string());
        let Path(id) = Path::<u64>::from_request(&req, "id").unwrap();
        assert_eq!(id, 42u64);
    }

    #[test]
    fn path_missing_param_returns_error() {
        let req = Request::new("GET", "/users/42");
        assert!(Path::<u64>::from_request(&req, "id").is_err());
    }

    #[test]
    fn path_invalid_type_returns_error() {
        let mut req = Request::new("GET", "/users/abc");
        req.path_params.insert("id".to_string(), "abc".to_string());
        assert!(Path::<u64>::from_request(&req, "id").is_err());
    }

    #[test]
    fn path_deref_works() {
        let p = Path(99u32);
        assert_eq!(*p, 99u32);
    }

    // --- ProblemDetail ---

    #[test]
    fn problem_detail_not_found_status_404() {
        let resp = ProblemDetail::not_found("user 42 not found");
        assert_eq!(resp.status.0, 404);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["status"], 404);
        assert_eq!(body["title"], "Not Found");
        assert_eq!(body["detail"], "user 42 not found");
    }

    #[test]
    fn problem_detail_bad_request_status_400() {
        let resp = ProblemDetail::bad_request("invalid id");
        assert_eq!(resp.status.0, 400);
        let body: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(body["status"], 400);
    }

    #[test]
    fn problem_detail_internal_error_status_500() {
        let resp = ProblemDetail::internal_error("db connection failed");
        assert_eq!(resp.status.0, 500);
    }
}
