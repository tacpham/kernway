//! todo-app — Kernway v1.0 Flagship Example
//!
//! A production-style Todo management API demonstrating all Kernway features:
//!   DI, ORM, Cache, Middleware, OpenAPI 3.0, SSE
//!
//! Run:  .\kw.ps1 run todo-app
//! API:
//!   GET    /health              — health check with stats
//!   GET    /openapi.json        — OpenAPI 3.0 spec
//!   GET    /todos               — list all todos (supports ?done=true/false)
//!   GET    /todos/{id}          — get todo by id (cached 60s)
//!   POST   /todos               — create todo
//!   PUT    /todos/{id}          — update todo
//!   PATCH  /todos/{id}/complete — mark as done
//!   DELETE /todos/{id}          — delete todo
//!   GET    /events              — SSE stream of change events

use di_core::AppContext;
use kernway_cache_core::{Cache, Ttl};
use kernway_cache_memory::InMemoryCache;
use kernway_core::{error::StatusCode, response::IntoResponse};
use kernway_openapi::{OpenApiRegistry, RouteDoc};
use kernway_orm_core::repository::Repository;
use kernway_orm_macro::entity;
use kernway_orm_memory::InMemoryRepository;
use kernway_server::{
    middleware::{LoggingMiddleware, RequestIdMiddleware},
    KernwayApp,
};
use kernway_sse::{SseEvent, SseStream};
use kernway_web::{Json, Path, ProblemDetail};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_str() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}", t.as_secs())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[entity(table = "todos")]
pub struct Todo {
    #[id(strategy = "auto")]
    pub id: u64,
    pub title: String,
    pub description: String,
    pub priority: u8,
    pub done: bool,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateTodo {
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTodo {
    pub title: Option<String>,
    pub description: Option<String>,
    pub priority: Option<u8>,
}

pub struct TodoRepository {
    inner: Arc<InMemoryRepository<Todo>>,
}

impl Default for TodoRepository {
    fn default() -> Self { Self::new() }
}

impl TodoRepository {
    pub fn new() -> Self {
        Self { inner: Arc::new(InMemoryRepository::new()) }
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

pub struct EventBus {
    events: Mutex<Vec<String>>,
}

impl Default for EventBus {
    fn default() -> Self { Self::new() }
}

impl EventBus {
    pub fn new() -> Self {
        Self { events: Mutex::new(Vec::new()) }
    }

    pub fn publish(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }

    pub fn drain(&self) -> Vec<String> {
        let mut events = self.events.lock().unwrap();
        std::mem::take(&mut *events)
    }
}

pub struct TodoService {
    repo: Arc<TodoRepository>,
    bus: Arc<EventBus>,
    cache: Arc<InMemoryCache<u64, Todo>>,
}

impl TodoService {
    pub fn new_with_deps(repo: Arc<TodoRepository>, bus: Arc<EventBus>) -> Self {
        Self {
            repo,
            bus,
            cache: Arc::new(InMemoryCache::new()),
        }
    }

    pub fn list(&self, done_filter: Option<bool>) -> Vec<Todo> {
        let all = self.repo.find_all();
        match done_filter {
            Some(filter) => all.into_iter().filter(|todo| todo.done == filter).collect(),
            None => all,
        }
    }

    pub fn get(&self, id: u64) -> Option<Todo> {
        if let Ok(Some(cached)) = self.cache.get(&id) {
            return Some(cached);
        }

        let todo = self.repo.find_by_id(id)?;
        let _ = self.cache.put(id, todo.clone(), Ttl::minutes(1));
        Some(todo)
    }

    pub fn create(&self, req: CreateTodo) -> Todo {
        let todo = self.repo.save(Todo {
            id: 0,
            title: req.title,
            description: req.description.unwrap_or_default(),
            priority: req.priority.unwrap_or(2),
            done: false,
            created_at: now_str(),
        });
        self.bus.publish(format!("todo.created:{}", todo.id));
        todo
    }

    pub fn update(&self, id: u64, req: UpdateTodo) -> Option<Todo> {
        let mut todo = self.repo.find_by_id(id)?;
        if let Some(title) = req.title {
            todo.title = title;
        }
        if let Some(description) = req.description {
            todo.description = description;
        }
        if let Some(priority) = req.priority {
            todo.priority = priority;
        }
        let saved = self.repo.save(todo);
        let _ = self.cache.evict(&id);
        self.bus.publish(format!("todo.updated:{}", id));
        Some(saved)
    }

    pub fn complete(&self, id: u64) -> Option<Todo> {
        let mut todo = self.repo.find_by_id(id)?;
        todo.done = true;
        let saved = self.repo.save(todo);
        let _ = self.cache.evict(&id);
        self.bus.publish(format!("todo.completed:{}", id));
        Some(saved)
    }

    pub fn delete(&self, id: u64) -> bool {
        let deleted = self.repo.delete_by_id(id);
        if deleted {
            let _ = self.cache.evict(&id);
            self.bus.publish(format!("todo.deleted:{}", id));
        }
        deleted
    }

    pub fn count(&self) -> u64 {
        self.repo.count()
    }
}

fn main() {
    let start_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut ctx = AppContext::new();

    let repo = Arc::new(TodoRepository::new());
    ctx.register_instance::<TodoRepository>(Arc::clone(&repo)).unwrap();

    let bus = Arc::new(EventBus::new());
    ctx.register_instance::<EventBus>(Arc::clone(&bus)).unwrap();

    let service = TodoService::new_with_deps(repo, Arc::clone(&bus));
    ctx.register_instance::<TodoService>(Arc::new(service)).unwrap();

    {
        let svc = ctx.get::<TodoService>().unwrap();
        svc.create(CreateTodo {
            title: "Read Kernway docs".into(),
            description: Some("Learn the framework".into()),
            priority: Some(3),
        });
        svc.create(CreateTodo {
            title: "Build a REST API".into(),
            description: Some("Use kernway-web".into()),
            priority: Some(2),
        });
        svc.create(CreateTodo {
            title: "Write unit tests".into(),
            description: Some("Use #[cfg(test)]".into()),
            priority: Some(2),
        });
        bus.drain();
    }

    println!("✅ {} beans registered", ctx.bean_count());

    let mut openapi = OpenApiRegistry::new("Kernway Todo API", "1.0.0")
        .description("Full-featured Todo API — Kernway v1.0 flagship example");

    openapi.add_route(
        RouteDoc::new("Health check")
            .tag("system")
            .response_json(200, "Server health + stats", "#/components/schemas/Health"),
        "GET", "/health",
    );
    openapi.add_route(
        RouteDoc::new("OpenAPI specification")
            .tag("system")
            .response(200, "OpenAPI 3.0 JSON"),
        "GET", "/openapi.json",
    );
    openapi.add_route(
        RouteDoc::new("List todos")
            .tag("todos")
            .query_param("done", "Filter by completion status", "boolean", false)
            .response_json(200, "Todo list", "#/components/schemas/Todo"),
        "GET", "/todos",
    );
    openapi.add_route(
        RouteDoc::new("Get todo by ID")
            .tag("todos")
            .path_param("id", "Todo ID", "integer")
            .response_json(200, "Todo found", "#/components/schemas/Todo")
            .response(404, "Not found"),
        "GET", "/todos/{id}",
    );
    openapi.add_route(
        RouteDoc::new("Create todo")
            .tag("todos")
            .body_json("Todo to create", "#/components/schemas/CreateTodo")
            .response_json(201, "Created", "#/components/schemas/Todo"),
        "POST", "/todos",
    );
    openapi.add_route(
        RouteDoc::new("Update todo")
            .tag("todos")
            .path_param("id", "Todo ID", "integer")
            .body_json("Fields to update", "#/components/schemas/UpdateTodo")
            .response_json(200, "Updated", "#/components/schemas/Todo")
            .response(404, "Not found"),
        "PUT", "/todos/{id}",
    );
    openapi.add_route(
        RouteDoc::new("Complete todo")
            .tag("todos")
            .path_param("id", "Todo ID", "integer")
            .response_json(200, "Marked complete", "#/components/schemas/Todo")
            .response(404, "Not found"),
        "PATCH", "/todos/{id}/complete",
    );
    openapi.add_route(
        RouteDoc::new("Delete todo")
            .tag("todos")
            .path_param("id", "Todo ID", "integer")
            .response(204, "Deleted")
            .response(404, "Not found"),
        "DELETE", "/todos/{id}",
    );
    openapi.add_route(
        RouteDoc::new("Change event stream (SSE)")
            .tag("events")
            .response(200, "SSE: todo.created|updated|completed|deleted events"),
        "GET", "/events",
    );

    let openapi_json = Arc::new(openapi.to_json());

    println!("📖 OpenAPI: http://localhost:8080/openapi.json");
    println!("🚀 Kernway Todo API v1.0");

    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .context(ctx)
        .layer(RequestIdMiddleware)
        .layer(LoggingMiddleware)
        .get("/health", move |_req, ctx| {
            let svc = ctx.get::<TodoService>().unwrap();
            let uptime = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(start_time);
            Json(serde_json::json!({
                "status": "UP",
                "version": "1.0.0",
                "uptime_s": uptime,
                "todos": svc.count(),
            }))
            .into_response()
        })
        .get("/openapi.json", {
            let json = Arc::clone(&openapi_json);
            move |_req, _ctx| {
                kernway_core::response::Response::new(StatusCode::OK)
                    .content_type("application/json; charset=utf-8")
                    .body(json.as_bytes().to_vec())
            }
        })
        .get("/todos", |req, ctx| {
            let done_filter = req.query.get("done").and_then(|value| match value {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            });
            Json(ctx.get::<TodoService>().unwrap().list(done_filter)).into_response()
        })
        .get("/todos/{id}", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(path) => *path,
                Err(err) => return ProblemDetail::bad_request(err),
            };
            match ctx.get::<TodoService>().unwrap().get(id) {
                Some(todo) => Json(todo).into_response(),
                None => ProblemDetail::not_found(format!("todo {} not found", id)),
            }
        })
        .post("/todos", |req, ctx| {
            let body: CreateTodo = match serde_json::from_slice(&req.body) {
                Ok(body) => body,
                Err(err) => return ProblemDetail::bad_request(format!("invalid body: {}", err)),
            };
            let todo = ctx.get::<TodoService>().unwrap().create(body);
            let mut resp = Json(todo).into_response();
            resp.status = StatusCode::CREATED;
            resp
        })
        .put("/todos/{id}", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(path) => *path,
                Err(err) => return ProblemDetail::bad_request(err),
            };
            let body: UpdateTodo = match serde_json::from_slice(&req.body) {
                Ok(body) => body,
                Err(err) => return ProblemDetail::bad_request(format!("invalid body: {}", err)),
            };
            match ctx.get::<TodoService>().unwrap().update(id, body) {
                Some(todo) => Json(todo).into_response(),
                None => ProblemDetail::not_found(format!("todo {} not found", id)),
            }
        })
        .patch("/todos/{id}/complete", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(path) => *path,
                Err(err) => return ProblemDetail::bad_request(err),
            };
            match ctx.get::<TodoService>().unwrap().complete(id) {
                Some(todo) => Json(todo).into_response(),
                None => ProblemDetail::not_found(format!("todo {} not found", id)),
            }
        })
        .delete("/todos/{id}", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(path) => *path,
                Err(err) => return ProblemDetail::bad_request(err),
            };
            if ctx.get::<TodoService>().unwrap().delete(id) {
                StatusCode::NO_CONTENT.into_response()
            } else {
                ProblemDetail::not_found(format!("todo {} not found", id))
            }
        })
        .get("/events", |_req, ctx| {
            let events = ctx.get::<EventBus>().unwrap().drain();
            let mut all = vec![SseEvent::named(
                "connected",
                r#"{"service":"todo-api","version":"1.0.0"}"#,
            )];
            let sse_events = events.iter().enumerate().map(|(i, event)| {
                let (event_type, data) = event.split_once(':').unwrap_or(("event", event.as_str()));
                SseEvent::with_id((i + 1).to_string(), event_type, data)
            });
            all.extend(sse_events);
            SseStream::new(all).into_response()
        })
        .build()
        .run()
        .expect("server failed to start");
}
