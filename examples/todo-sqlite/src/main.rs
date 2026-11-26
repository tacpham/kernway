//! todo-sqlite — a REST todo list stored in a SQLite file.
//!
//! Run: `cargo run -p todo-sqlite` (file: `TODO_SQLITE_DB`, default
//! `examples/todo-sqlite/todos.db`; address: `TODO_ADDR`, default
//! `0.0.0.0:8080`), then
//! `curl -X POST localhost:8080/todos -d '{"title":"buy milk"}'`.
//!
//! The driver is per shard (`!Send`, like every kernway driver): each worker
//! thread opens it on first use and keeps it in a `thread_local`. Handlers
//! only capture the path. Every call is async — SQLite runs on the driver's
//! connection threads, never on the shard.

use kernway_core::{error::StatusCode, request::Request, response::IntoResponse};
use kernway_orm_core::Driver;
use kernway_orm_macro::entity;
use kernway_orm_sqlite::SqliteDriver;
use kernway_server::{middleware::LoggingMiddleware, KernwayApp};
use kernway_web::{Json, Path, ProblemDetail};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[entity(table = "todos")]
pub struct Todo {
    #[id(strategy = "auto")]
    pub id: i64,
    pub title: String,
    pub done: bool,
}

thread_local! {
    static DB: RefCell<Option<Rc<SqliteDriver>>> = const { RefCell::new(None) };
}

/// This shard's driver, opened (and the table created) on first use.
async fn db(path: &str) -> Rc<SqliteDriver> {
    if let Some(d) = DB.with(|c| c.borrow().clone()) {
        return d;
    }
    let d = Rc::new(SqliteDriver::open(path).await.expect("open the SQLite file"));
    d.ensure_table::<Todo>().await.expect("create the todos table");
    DB.with(|c| *c.borrow_mut() = Some(Rc::clone(&d)));
    d
}

fn id_of(req: &Request) -> Result<i64, kernway_core::response::Response> {
    Path::<i64>::from_request(req, "id").map(|p| *p).map_err(ProblemDetail::bad_request)
}

#[derive(Deserialize)]
struct CreateTodo {
    title: String,
}

fn main() {
    let path: Arc<str> = std::env::var("TODO_SQLITE_DB").unwrap_or_else(|_| "examples/todo-sqlite/todos.db".to_string()).into();
    println!("SQLite file: {path}");
    let p = |path: &Arc<str>| Arc::clone(path);

    KernwayApp::builder()
        .bind(&std::env::var("TODO_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string()))
        .layer(LoggingMiddleware::new())
        .get("/todos", {
            let path = p(&path);
            move |_req: Request, _ctx: &kernway_server::RequestScope| {
                let path = Arc::clone(&path);
                async move {
                    match db(&path).await.repository::<Todo>().find_all().await {
                        Ok(all) => Json(all).into_response(),
                        Err(e) => ProblemDetail::internal_error(e.to_string()),
                    }
                }
            }
        })
        .post("/todos", {
            let path = p(&path);
            move |req: Request, _ctx: &kernway_server::RequestScope| {
                let path = Arc::clone(&path);
                async move {
                    let body = match Json::<CreateTodo>::from_request(&req) {
                        Ok(Json(body)) => body,
                        Err(err) => return ProblemDetail::bad_request(err),
                    };
                    let todo = Todo { id: 0, title: body.title, done: false };
                    match db(&path).await.repository::<Todo>().save(todo).await {
                        Ok(saved) => (StatusCode::CREATED, Json(saved)).into_response(),
                        Err(e) => ProblemDetail::internal_error(e.to_string()),
                    }
                }
            }
        })
        .patch("/todos/{id}/done", {
            let path = p(&path);
            move |req: Request, _ctx: &kernway_server::RequestScope| {
                let path = Arc::clone(&path);
                async move {
                    let id = match id_of(&req) {
                        Ok(id) => id,
                        Err(r) => return r,
                    };
                    let repo = db(&path).await.repository::<Todo>();
                    match repo.find_by_id(&id).await {
                        Ok(Some(mut todo)) => {
                            todo.done = true;
                            match repo.save(todo).await {
                                Ok(t) => Json(t).into_response(),
                                Err(e) => ProblemDetail::internal_error(e.to_string()),
                            }
                        }
                        Ok(None) => ProblemDetail::not_found(format!("todo {id} not found")),
                        Err(e) => ProblemDetail::internal_error(e.to_string()),
                    }
                }
            }
        })
        .delete("/todos/{id}", {
            let path = p(&path);
            move |req: Request, _ctx: &kernway_server::RequestScope| {
                let path = Arc::clone(&path);
                async move {
                    let id = match id_of(&req) {
                        Ok(id) => id,
                        Err(r) => return r,
                    };
                    let repo = db(&path).await.repository::<Todo>();
                    match repo.exists_by_id(&id).await {
                        Ok(true) => match repo.delete_by_id(&id).await {
                            Ok(()) => StatusCode::NO_CONTENT.into_response(),
                            Err(e) => ProblemDetail::internal_error(e.to_string()),
                        },
                        Ok(false) => ProblemDetail::not_found(format!("todo {id} not found")),
                        Err(e) => ProblemDetail::internal_error(e.to_string()),
                    }
                }
            }
        })
        .get("/db-info", {
            let path = p(&path);
            move |_req: Request, _ctx: &kernway_server::RequestScope| {
                let path = Arc::clone(&path);
                async move {
                    let count = db(&path).await.repository::<Todo>().count().await.unwrap_or(0);
                    Json(serde_json::json!({ "backend": "sqlite", "db_path": &*path, "count": count })).into_response()
                }
            }
        })
        .build()
        .run()
        .expect("server failed to start");
}
