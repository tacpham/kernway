//! hello-openapi — Kernway v0.6: OpenAPI + SSE + Multipart
//!
//! Run:  .\kw.ps1 run hello-openapi
//! Test:
//!   curl http://localhost:8080/openapi.json
//!   curl http://localhost:8080/users
//!   curl http://localhost:8080/events
//!   curl -X POST http://localhost:8080/users -H "Content-Type: application/json" -d '{"name":"Dave"}'

use di_core::AppContext;
use di_macro::Component;
use kernway_core::{error::StatusCode, response::IntoResponse};
use kernway_openapi::{OpenApiRegistry, RouteDoc};
use kernway_server::{
    middleware::{LoggingMiddleware, RequestIdMiddleware},
    KernwayApp,
};
use kernway_sse::{SseEvent, SseStream};
use kernway_web::{Json, Path, ProblemDetail};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id:   u64,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateUser {
    pub name: String,
}

#[derive(Component)]
pub struct UserService {
    users:   Mutex<Vec<User>>,
    counter: Mutex<u64>,
}

impl Default for UserService {
    fn default() -> Self { Self::new() }
}

impl UserService {
    pub fn new() -> Self {
        Self {
            users: Mutex::new(vec![
                User { id: 1, name: "Alice".into() },
                User { id: 2, name: "Bob".into() },
            ]),
            counter: Mutex::new(2),
        }
    }

    pub fn list(&self) -> Vec<User> { self.users.lock().unwrap().clone() }

    pub fn get(&self, id: u64) -> Option<User> {
        self.users.lock().unwrap().iter().find(|u| u.id == id).cloned()
    }

    pub fn create(&self, name: String) -> User {
        let mut counter = self.counter.lock().unwrap();
        *counter += 1;
        let user = User { id: *counter, name };
        self.users.lock().unwrap().push(user.clone());
        user
    }

    pub fn delete(&self, id: u64) -> bool {
        let mut users = self.users.lock().unwrap();
        let before = users.len();
        users.retain(|u| u.id != id);
        users.len() < before
    }
}

fn main() {
    let mut ctx = AppContext::new();
    ctx.register_instance::<UserService>(Arc::new(UserService::new())).unwrap();

    let mut openapi = OpenApiRegistry::new("Kernway User API", "0.6.0")
        .description("Demo API with OpenAPI 3.0, SSE, and Multipart support");

    openapi.add_route(
        RouteDoc::new("List all users").tag("users").response_json(200, "User list", "#/components/schemas/User"),
        "GET", "/users",
    );
    openapi.add_route(
        RouteDoc::new("Get user by ID").tag("users")
            .path_param("id", "User ID", "integer")
            .response_json(200, "User found", "#/components/schemas/User")
            .response(404, "User not found"),
        "GET", "/users/{id}",
    );
    openapi.add_route(
        RouteDoc::new("Create user").tag("users")
            .body_json("User to create", "#/components/schemas/CreateUser")
            .response_json(201, "User created", "#/components/schemas/User"),
        "POST", "/users",
    );
    openapi.add_route(
        RouteDoc::new("Delete user").tag("users")
            .path_param("id", "User ID", "integer")
            .response(204, "Deleted")
            .response(404, "Not found"),
        "DELETE", "/users/{id}",
    );
    openapi.add_route(
        RouteDoc::new("Server-Sent Events stream").tag("events")
            .response(200, "SSE stream — text/event-stream"),
        "GET", "/events",
    );

    let openapi_json = Arc::new(openapi.to_json());
    println!("✅ {} beans registered", ctx.bean_count());
    println!("📖 OpenAPI spec ready — visit http://localhost:8080/openapi.json");

    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .context(ctx)
        .layer(RequestIdMiddleware)
        .layer(LoggingMiddleware)
        .get("/openapi.json", {
            let json = Arc::clone(&openapi_json);
            move |_req, _ctx| {
                kernway_core::response::Response::new(StatusCode::OK)
                    .content_type("application/json; charset=utf-8")
                    .body(json.as_bytes().to_vec())
            }
        })
        .get("/users", |_req, ctx| {
            let svc = ctx.get::<UserService>().unwrap();
            Json(svc.list()).into_response()
        })
        .get("/users/{id}", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(p) => *p,
                Err(e) => return ProblemDetail::bad_request(e),
            };
            match ctx.get::<UserService>().unwrap().get(id) {
                Some(u) => Json(u).into_response(),
                None => ProblemDetail::not_found(format!("user {} not found", id)),
            }
        })
        .post("/users", |req, ctx| {
            let body: CreateUser = match serde_json::from_slice(&req.body) {
                Ok(b) => b,
                Err(e) => return ProblemDetail::bad_request(format!("invalid body: {}", e)),
            };
            let user = ctx.get::<UserService>().unwrap().create(body.name);
            let mut resp = Json(user).into_response();
            resp.status = StatusCode::CREATED;
            resp
        })
        .delete("/users/{id}", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(p) => *p,
                Err(e) => return ProblemDetail::bad_request(e),
            };
            if ctx.get::<UserService>().unwrap().delete(id) {
                StatusCode::NO_CONTENT.into_response()
            } else {
                ProblemDetail::not_found(format!("user {} not found", id))
            }
        })
        .get("/events", |_req, _ctx| {
            SseStream::new(vec![
                SseEvent::data("connected").retry(5000),
                SseEvent::with_id("1", "user.created", r#"{"id":3,"name":"Charlie"}"#),
                SseEvent::with_id("2", "user.updated", r#"{"id":1,"name":"Alice Smith"}"#),
                SseEvent::named("heartbeat", "{}"),
            ])
            .into_response()
        })
        .build()
        .run();
}
