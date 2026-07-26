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

// ============================================================
// Html<T> — HTML response (a full page or an htmx fragment)
// ============================================================

/// An HTML response — `Content-Type: text/html; charset=utf-8`.
///
/// Used for a full page or an htmx fragment alike; the difference is only how
/// much markup it carries. The value is sent verbatim, so an engine that
/// produced it is responsible for escaping — this type does not escape.
///
/// ```rust,ignore
/// fn page(req: &Request, ctx: &AppContext) -> Html<String> {
///     Html("<h1>Hello</h1>".to_string())
/// }
/// ```
pub struct Html<T>(pub T);

impl<T: Into<String> + Send> IntoResponse for Html<T> {
    fn into_response(self) -> Response {
        Response::new(StatusCode::OK)
            .content_type("text/html; charset=utf-8")
            .body(self.0.into().into_bytes())
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

// ============================================================
// Validated<T> — deserialize + validate a request body
// ============================================================

use kernway_validation::{Validate, ValidationErrors};

/// A request body deserialized **and** validated ([`kernway_validation`]).
///
/// [`from_request`](Validated::from_request) parses the JSON body and runs the
/// type's `Validate`; on failure it returns a ready RFC 7807 `400` listing every
/// field error, so a handler is:
///
/// ```rust,ignore
/// let Validated(body) = match Validated::<CreateUser>::from_request(&req) {
///     Ok(v) => v,
///     Err(resp) => return resp,   // 400 with the field errors
/// };
/// // `body` is now known-valid
/// ```
pub struct Validated<T>(pub T);

impl<T: DeserializeOwned + Validate> Validated<T> {
    /// Deserialize the JSON body into `T` and validate it. The `Err` is a finished
    /// `400` response — a malformed body is a plain problem, a validation failure
    /// carries the per-field `errors`.
    ///
    /// # Errors
    /// Returns a `400` [`Response`] when the body is not valid JSON for `T` or fails
    /// validation.
    pub fn from_request(req: &Request) -> Result<Self, Response> {
        let value: T = serde_json::from_slice(&req.body)
            .map_err(|e| ProblemDetail::bad_request(format!("invalid JSON body: {e}")))?;
        match value.validate() {
            Ok(()) => Ok(Validated(value)),
            Err(errors) => Err(validation_response(&errors)),
        }
    }
}

impl<T> std::ops::Deref for Validated<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// One field failure in the RFC 7807 `errors` extension member.
#[derive(Serialize)]
struct FieldProblem<'a> {
    field: &'a str,
    message: &'a str,
}

/// RFC 7807 problem with a validation `errors` array (an extension member, §3.2).
#[derive(Serialize)]
struct ValidationProblem<'a> {
    status: u16,
    title: &'static str,
    detail: &'static str,
    errors: Vec<FieldProblem<'a>>,
}

/// A `400` response listing every field failure.
fn validation_response(errors: &ValidationErrors) -> Response {
    let problem = ValidationProblem {
        status: 400,
        title: "Validation Failed",
        detail: "the request body did not pass validation",
        errors: errors
            .errors()
            .iter()
            .map(|e| FieldProblem { field: &e.field, message: &e.message })
            .collect(),
    };
    let mut response = Json(problem).into_response();
    response.status = StatusCode::BAD_REQUEST;
    response
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
        let parsed: User = serde_json::from_slice(resp.body_bytes()).unwrap();
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

    // --- Validated<T> ---

    #[test]
    fn validated_accepts_valid_and_rejects_invalid_with_field_errors() {
        use kernway_validation::{rules, Validate, ValidationErrors};

        #[derive(serde::Deserialize)]
        struct NewUser {
            name: String,
            email: String,
        }
        impl Validate for NewUser {
            fn validate(&self) -> Result<(), ValidationErrors> {
                let mut errors = ValidationErrors::new();
                if let Err(m) = rules::not_blank(&self.name) {
                    errors.push("name", m);
                }
                if let Err(m) = rules::email(&self.email) {
                    errors.push("email", m);
                }
                errors.into_result()
            }
        }

        // A valid body passes through.
        let mut req = Request::new("POST", "/users");
        req.body = br#"{"name":"Alice","email":"alice@example.com"}"#.to_vec();
        assert!(Validated::<NewUser>::from_request(&req).is_ok());

        // An invalid body → 400 listing every field error.
        let mut bad = Request::new("POST", "/users");
        bad.body = br#"{"name":"","email":"nope"}"#.to_vec();
        let resp = Validated::<NewUser>::from_request(&bad).err().unwrap();
        assert_eq!(resp.status, StatusCode::BAD_REQUEST);
        let body = String::from_utf8_lossy(resp.body_bytes());
        assert!(body.contains("Validation Failed"), "body: {body}");
        assert!(body.contains(r#""field":"name""#), "name error present: {body}");
        assert!(body.contains(r#""field":"email""#), "email error present: {body}");

        // A malformed body is a plain 400 problem.
        let mut malformed = Request::new("POST", "/users");
        malformed.body = b"not json".to_vec();
        assert_eq!(
            Validated::<NewUser>::from_request(&malformed).err().unwrap().status,
            StatusCode::BAD_REQUEST
        );
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
        let body: serde_json::Value = serde_json::from_slice(resp.body_bytes()).unwrap();
        assert_eq!(body["status"], 404);
        assert_eq!(body["title"], "Not Found");
        assert_eq!(body["detail"], "user 42 not found");
    }

    #[test]
    fn problem_detail_bad_request_status_400() {
        let resp = ProblemDetail::bad_request("invalid id");
        assert_eq!(resp.status.0, 400);
        let body: serde_json::Value = serde_json::from_slice(resp.body_bytes()).unwrap();
        assert_eq!(body["status"], 400);
    }

    #[test]
    fn problem_detail_internal_error_status_500() {
        let resp = ProblemDetail::internal_error("db connection failed");
        assert_eq!(resp.status.0, 500);
    }
}
