use di_core::AppContext;
use kernway_core::{error::StatusCode, response::IntoResponse};
use kernway_orm_core::repository::Repository;
use kernway_orm_macro::entity;
use kernway_orm_sqlite::SqliteRepository;
use kernway_server::{middleware::LoggingMiddleware, KernwayApp};
use kernway_web::{Json, Path, ProblemDetail};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[entity(table = "todos")]
pub struct Todo {
    #[id(strategy = "auto")]
    pub id: u64,
    pub title: String,
    pub done: bool,
}

pub struct TodoRepository {
    inner: Arc<dyn Repository<Todo>>,
}

impl TodoRepository {
    pub fn new(inner: Arc<dyn Repository<Todo>>) -> Self {
        Self { inner }
    }

    pub fn find_all(&self) -> Vec<Todo> {
        self.inner.find_all().unwrap_or_default()
    }

    pub fn find_by_id(&self, id: u64) -> Option<Todo> {
        self.inner.find_by_id(&id).unwrap_or(None)
    }

    pub fn save(&self, todo: Todo) -> Todo {
        self.inner.save(todo).unwrap()
    }

    pub fn delete_by_id(&self, id: u64) -> bool {
        if self.inner.exists_by_id(&id).unwrap_or(false) {
            self.inner.delete_by_id(&id).unwrap();
            true
        } else {
            false
        }
    }

    pub fn count(&self) -> u64 {
        self.inner.count().unwrap_or(0)
    }
}

pub struct TodoService {
    repo: Arc<TodoRepository>,
}

impl TodoService {
    pub fn new(repo: Arc<TodoRepository>) -> Self {
        Self { repo }
    }

    pub fn list(&self) -> Vec<Todo> {
        self.repo.find_all()
    }

    pub fn create(&self, title: String) -> Todo {
        self.repo.save(Todo {
            id: 0,
            title,
            done: false,
        })
    }

    pub fn mark_done(&self, id: u64) -> Option<Todo> {
        let mut todo = self.repo.find_by_id(id)?;
        todo.done = true;
        Some(self.repo.save(todo))
    }

    pub fn delete(&self, id: u64) -> bool {
        self.repo.delete_by_id(id)
    }

    pub fn count(&self) -> u64 {
        self.repo.count()
    }
}

#[derive(Deserialize)]
struct CreateTodo {
    title: String,
}

fn main() {
    let db_path = std::env::var("TODO_SQLITE_DB")
        .unwrap_or_else(|_| "examples/todo-sqlite/todos.db".to_string());
    let db_info_path = db_path.clone();

    // --- BEFORE (InMemoryRepository) ---
    // let repo: Arc<dyn Repository<Todo>> = Arc::new(InMemoryRepository::new());
    //
    // --- AFTER (SqliteRepository) — zero service changes ---
    let repo: Arc<dyn Repository<Todo>> = Arc::new(SqliteRepository::<Todo>::open(&db_path).unwrap());

    let mut ctx = AppContext::new();
    let todo_repo = Arc::new(TodoRepository::new(repo));
    ctx.register_instance::<TodoRepository>(Arc::clone(&todo_repo)).unwrap();
    ctx.register_instance::<TodoService>(Arc::new(TodoService::new(todo_repo))).unwrap();

    println!("🗃️  SQLite file: {}", db_path);

    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .context(ctx)
        .layer(LoggingMiddleware)
        .get("/todos", |_req, ctx| {
            let svc = ctx.get::<TodoService>().unwrap();
            Json(svc.list()).into_response()
        })
        .post("/todos", |req, ctx| {
            let body = match Json::<CreateTodo>::from_request(req) {
                Ok(Json(body)) => body,
                Err(err) => return ProblemDetail::bad_request(err),
            };

            let svc = ctx.get::<TodoService>().unwrap();
            (StatusCode::CREATED, Json(svc.create(body.title))).into_response()
        })
        .patch("/todos/{id}/done", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(path) => *path,
                Err(err) => return ProblemDetail::bad_request(err),
            };

            let svc = ctx.get::<TodoService>().unwrap();
            match svc.mark_done(id) {
                Some(todo) => Json(todo).into_response(),
                None => ProblemDetail::not_found(format!("todo {} not found", id)),
            }
        })
        .delete("/todos/{id}", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(path) => *path,
                Err(err) => return ProblemDetail::bad_request(err),
            };

            let svc = ctx.get::<TodoService>().unwrap();
            if svc.delete(id) {
                StatusCode::NO_CONTENT.into_response()
            } else {
                ProblemDetail::not_found(format!("todo {} not found", id))
            }
        })
        .get("/db-info", {
            let db_path = db_info_path.clone();
            move |_req, ctx| {
                let svc = ctx.get::<TodoService>().unwrap();
                Json(serde_json::json!({
                    "backend": "sqlite",
                    "db_path": db_path,
                    "count": svc.count(),
                }))
                .into_response()
            }
        })
        .build()
        .run()
        .expect("server failed to start");
}
