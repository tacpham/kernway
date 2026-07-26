//! hello-validate — request validation at the web boundary, over a real socket.
//!
//! `POST /users` with a JSON body:
//! - valid   → `201 Created`.
//! - invalid → `400` RFC 7807 listing every field that failed.
//! - not JSON → `400` problem.
//!
//! The point: validation runs in the `Validated<T>` extractor, *before* the handler
//! body, and is independent of what the handler then does with the data.

use kernway::prelude::*;

/// The request body. `#[derive(Validate)]` generates the field checks from the
/// `#[validate(...)]` attributes; the `Validated<T>` extractor runs them.
#[derive(serde::Deserialize, Validate)]
struct CreateUser {
    #[validate(not_blank, length(min = 3, max = 50))]
    name: String,
    #[validate(email)]
    email: String,
    #[validate(range(min = 0, max = 150))]
    age: u8,
}

/// Build the app bound to `addr`. Shared by the binary and the socket test.
pub fn build_app(addr: &str) -> KernwayApp {
    KernwayApp::builder()
        .bind(addr)
        .post("/users", |req: Request, _scope: &RequestScope| async move {
            // Validation happens HERE — the web boundary — before any logic runs.
            // On failure the extractor hands back a finished 400, which we return.
            let user = match Validated::<CreateUser>::from_request(&req) {
                Ok(Validated(user)) => user,
                Err(problem) => return problem, // 400 RFC 7807 with per-field errors
            };

            // `user` is now known-valid. Do whatever with it — an ORM, a raw
            // kernway-redis command, or nothing. Validation is decoupled from the
            // data layer; the handler never sees invalid input.
            (StatusCode::CREATED, Json(serde_json::json!({ "created": user.name }))).into_response()
        })
        .build()
}
