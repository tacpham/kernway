//! hello-cache — Kernway v0.5: Cache Layer
//!
//! Demonstrates: InMemoryCache + manual cache-aside pattern
//! (full AOP codegen for #[cacheable] arrives in v0.6)
//!
//! Run:   .\kw.ps1 run hello-cache
//! Test:  curl http://localhost:8080/users/1      # miss → loads from "DB"
//!        curl http://localhost:8080/users/1      # hit  → from cache
//!        curl http://localhost:8080/cache/stats  # hit/miss stats
//!        curl -X DELETE http://localhost:8080/users/1/cache  # evict

use di_core::AppContext;
use di_macro::Component;
use kernway_cache_core::{Cache, Ttl};
use kernway_cache_macro::{cache_evict, cacheable};
use kernway_cache_memory::InMemoryCache;
use kernway_core::{error::StatusCode, response::IntoResponse};
use kernway_server::{
    middleware::{LoggingMiddleware, RequestIdMiddleware},
    KernwayApp,
};
use kernway_web::{Json, Path, ProblemDetail};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: u64,
    pub name: String,
    pub email: String,
}

#[derive(Component)]
pub struct UserRepository;

impl UserRepository {
    pub fn find_by_id(&self, id: u64) -> Option<User> {
        println!("  [DB] Loading user {} from database...", id);
        match id {
            1 => Some(User { id: 1, name: "Alice".into(), email: "alice@example.com".into() }),
            2 => Some(User { id: 2, name: "Bob".into(), email: "bob@example.com".into() }),
            _ => None,
        }
    }
}

#[derive(Component)]
pub struct UserService {
    #[inject]
    repo: Arc<UserRepository>,
    cache: Arc<InMemoryCache<String, String>>,
}

impl UserService {
    pub fn new_with_cache(repo: Arc<UserRepository>) -> Self {
        Self { repo, cache: Arc::new(InMemoryCache::new()) }
    }

    /// Cache-aside: check cache first, fall back to DB.
    /// In v0.6, replace this boilerplate with #[cacheable(key = "user_{id}", ttl = 60)].
    #[cacheable(key = "user_{id}", ttl = 60)]
    pub fn get_user(&self, id: u64) -> Option<User> {
        let cache_key = format!("user:{}", id);
        let ttl = Ttl::minutes(1);

        if let Ok(Some(json)) = self.cache.get(&cache_key) {
            println!("  [CACHE HIT] user:{}", id);
            return serde_json::from_str(&json).ok();
        }

        let user = self.repo.find_by_id(id)?;

        if let Ok(json) = serde_json::to_string(&user) {
            let _ = self.cache.put(cache_key, json, ttl);
        }
        Some(user)
    }

    /// Evict user from cache.
    /// In v0.6: #[cache_evict(key = "user_{id}")]
    #[cache_evict(key = "user_{id}")]
    pub fn invalidate_user(&self, id: u64) {
        let key = format!("user:{}", id);
        println!("  [CACHE EVICT] user:{}", id);
        let _ = self.cache.evict(&key);
    }

    pub fn cache_stats(&self) -> kernway_cache_core::stats::CacheStats {
        self.cache.stats()
    }
}

fn main() {
    let mut ctx = AppContext::new();

    ctx.build::<UserRepository>().unwrap();

    let repo = ctx.get::<UserRepository>().unwrap();
    let service = UserService::new_with_cache(repo);
    ctx.register_instance::<UserService>(Arc::new(service)).unwrap();

    println!("✅ {} beans registered", ctx.bean_count());
    println!("🚀 Kernway v0.5 Cache Demo");

    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .context(ctx)
        .layer(RequestIdMiddleware)
        .layer(LoggingMiddleware)
        .get("/users/{id}", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(p) => *p,
                Err(e) => return ProblemDetail::bad_request(e),
            };
            let svc = ctx.get::<UserService>().unwrap();
            match svc.get_user(id) {
                Some(u) => Json(u).into_response(),
                None => ProblemDetail::not_found(format!("user {} not found", id)),
            }
        })
        .delete("/users/{id}/cache", |req, ctx| {
            let id = match Path::<u64>::from_request(req, "id") {
                Ok(p) => *p,
                Err(e) => return ProblemDetail::bad_request(e),
            };
            let svc = ctx.get::<UserService>().unwrap();
            svc.invalidate_user(id);
            StatusCode::NO_CONTENT.into_response()
        })
        .get("/cache/stats", |_req, ctx| {
            let svc = ctx.get::<UserService>().unwrap();
            let s = svc.cache_stats();
            Json(serde_json::json!({
                "hits": s.hits,
                "misses": s.misses,
                "entries": s.entries,
                "hit_ratio": s.hit_ratio(),
            })).into_response()
        })
        .build()
        .run();
}
