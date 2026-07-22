use di_core::AppContext;
use di_macro::Component;
use kernway_core::response::IntoResponse;
use kernway_orm_core::repository::Repository;
use kernway_orm_macro::entity;
use kernway_orm_memory::InMemoryRepository;
use kernway_server::{
    middleware::{LoggingMiddleware, RequestIdMiddleware},
    KernwayApp,
};
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
    inner: Arc<InMemoryRepository<Todo>>,
}

impl Default for TodoRepository {
    fn default() -> Self { Self::new() }
}

impl TodoRepository {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(InMemoryRepository::new()),
        }
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

    pub fn delete_by_id(&self, id: u64) {
        let _ = self.inner.delete_by_id(&id);
    }
}

#[derive(Component)]
pub struct TodoService {
    #[inject]
    repo: Arc<TodoRepository>,
}

impl TodoService {
    pub fn list(&self) -> Vec<Todo> {
        self.repo.find_all()
    }

    pub fn get(&self, id: u64) -> Option<Todo> {
        self.repo.find_by_id(id)
    }

    pub fn create(&self, title: String) -> Todo {
        self.repo.save(Todo {
            id: 0,
            title,
            done: false,
        })
    }

    pub fn complete(&self, id: u64) -> Option<Todo> {
        let mut todo = self.repo.find_by_id(id)?;
        todo.done = true;
        Some(self.repo.save(todo))
    }

    pub fn delete(&self, id: u64) {
        self.repo.delete_by_id(id)
    }
}

fn main() {
    let mut ctx = AppContext::new();
    ctx.register_instance::<TodoRepository>(Arc::new(TodoRepository::new()))
        .unwrap();
    ctx.build::<TodoService>().unwrap();

    println!("✅ {} beans registered", ctx.bean_count());

    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .context(ctx)
        .layer(RequestIdMiddleware)
        .layer(LoggingMiddleware)
        .get("/todos", |_req, ctx| {
            let svc = ctx.get::<TodoService>().unwrap();
            Json(svc.list()).into_response()
        })
        .get("/todos/{id}", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(path) => *path,
                Err(err) => return ProblemDetail::bad_request(err),
            };
            let svc = ctx.get::<TodoService>().unwrap();
            match svc.get(id) {
                Some(todo) => Json(todo).into_response(),
                None => ProblemDetail::not_found(format!("todo {} not found", id)),
            }
        })
        .post("/todos", |req, ctx| {
            #[derive(Deserialize)]
            struct CreateTodo {
                title: String,
            }

            let body: CreateTodo = match serde_json::from_slice(&req.body) {
                Ok(body) => body,
                Err(err) => {
                    return ProblemDetail::bad_request(format!("invalid body: {}", err));
                }
            };

            let svc = ctx.get::<TodoService>().unwrap();
            Json(svc.create(body.title)).into_response()
        })
        .put("/todos/{id}/complete", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(path) => *path,
                Err(err) => return ProblemDetail::bad_request(err),
            };
            let svc = ctx.get::<TodoService>().unwrap();
            match svc.complete(id) {
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
            svc.delete(id);
            kernway_core::error::StatusCode::NO_CONTENT.into_response()
        })
        .build()
        .run()
        .expect("server failed to start");
}
