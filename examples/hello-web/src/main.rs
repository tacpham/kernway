//! hello-web — Kernway v0.3: real HTTP server
//!
//! Run: .\kw.ps1 run hello-web
//! Test: curl http://localhost:8080/users/1
//!       curl http://localhost:8080/users/99
//!       curl http://localhost:8080/health

use di_core::{AppContext, RequestScope};
use di_macro::Component;
use kernway_core::request::Request;
use kernway_core::response::IntoResponse;
use kernway_server::KernwayApp;
use kernway_web::{Json, Path, ProblemDetail};
use serde::Serialize;
use std::sync::Arc;

// ============================================================
// Model
// ============================================================

#[derive(Serialize, Clone)]
pub struct User {
    pub id: u64,
    pub name: String,
    pub role: String,
}

// ============================================================
// Repository
// ============================================================

#[derive(Component)]
pub struct UserRepository;

impl UserRepository {
    pub fn find_by_id(&self, id: u64) -> Option<User> {
        match id {
            1 => Some(User {
                id: 1,
                name: "Alice".into(),
                role: "ADMIN".into(),
            }),
            2 => Some(User {
                id: 2,
                name: "Bob".into(),
                role: "USER".into(),
            }),
            3 => Some(User {
                id: 3,
                name: "Charlie".into(),
                role: "USER".into(),
            }),
            _ => None,
        }
    }

    pub fn find_all(&self) -> Vec<User> {
        vec![
            User {
                id: 1,
                name: "Alice".into(),
                role: "ADMIN".into(),
            },
            User {
                id: 2,
                name: "Bob".into(),
                role: "USER".into(),
            },
            User {
                id: 3,
                name: "Charlie".into(),
                role: "USER".into(),
            },
        ]
    }
}

// ============================================================
// Service
// ============================================================

#[derive(Component)]
pub struct UserService {
    #[inject]
    repo: Arc<UserRepository>,
}

impl UserService {
    pub fn get_user(&self, id: u64) -> Option<User> {
        self.repo.find_by_id(id)
    }

    pub fn list_users(&self) -> Vec<User> {
        self.repo.find_all()
    }
}

// ============================================================
// Main
// ============================================================

fn main() {
    // --- DI setup ---
    let mut ctx = AppContext::new();
    ctx.build::<UserRepository>().unwrap();
    ctx.build::<UserService>().unwrap();

    println!("✅ {} beans registered", ctx.bean_count());

    // --- HTTP server ---
    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .context(ctx)
        // GET /health
        .get("/health", |_req: Request, _ctx: &RequestScope| async {
            Json(serde_json::json!({ "status": "UP", "version": "0.3.0" })).into_response()
        })
        // GET /users — list all
        .get("/users", |_req: Request, ctx: &RequestScope| {
            let svc = ctx.get::<UserService>().unwrap();
            async move { Json(svc.list_users()).into_response() }
        })
        // GET /users/{id} — get one
        .get("/users/{id}", |req: Request, ctx: &RequestScope| {
            let svc = ctx.get::<UserService>().unwrap();
            async move {
                let id = match Path::<u64>::from_request(&req, "id") {
                    Ok(p) => *p,
                    Err(e) => return ProblemDetail::bad_request(e),
                };

                match svc.get_user(id) {
                    Some(user) => Json(user).into_response(),
                    None => ProblemDetail::not_found(format!("user {} not found", id)),
                }
            }
        })
        .build()
        .run()
        .expect("server failed to start");
}
