//! hello-di-v2 — Kernway DI auto-wiring demo (v0.2)
//!
//! Run: .\kw.ps1 run hello-di-v2
//!
//! Compared with v0.1:
//!   v0.1: manual construction (UserService { repo: ctx.get()? })
//!   v0.2: auto-wiring (#[derive(Component)] + #[inject] + ctx.build::<T>())

use di_core::AppContext;
use di_macro::Component;
use std::sync::Arc;

// --- Bean definitions ---

/// Repository — no dependencies.
#[derive(Component)]
pub struct UserRepository;

impl UserRepository {
    pub fn find_by_id(&self, id: u64) -> Option<String> {
        match id {
            1 => Some("Alice".to_string()),
            2 => Some("Bob".to_string()),
            _ => None,
        }
    }
}

/// Service — automatically injects UserRepository.
#[derive(Component)]
pub struct UserService {
    #[inject]
    repo: Arc<UserRepository>,
}

impl UserService {
    pub fn get_user(&self, id: u64) -> String {
        self.repo
            .find_by_id(id)
            .unwrap_or_else(|| format!("User #{id} not found"))
    }
}

/// Controller — automatically injects UserService.
#[derive(Component)]
pub struct UserController {
    #[inject]
    service: Arc<UserService>,
}

impl UserController {
    pub fn handle(&self, id: u64) -> String {
        format!("HTTP 200: {}", self.service.get_user(id))
    }
}

// --- App bootstrap ---

fn main() {
    println!("=== Kernway v0.2 — Auto-wiring Demo ===\n");

    let mut ctx = AppContext::new();

    // Auto-wiring: ctx.build::<T>() calls T::build(ctx) automatically
    // Order: dependencies first, dependents afterward
    ctx.build::<UserRepository>().unwrap();
    ctx.build::<UserService>().unwrap();      // automatically gets UserRepository from ctx
    ctx.build::<UserController>().unwrap();   // automatically gets UserService from ctx

    println!("✅ {} beans registered (auto-wired)\n", ctx.bean_count());

    let ctrl = ctx.get::<UserController>().unwrap();

    println!("--- Simulated HTTP requests ---");
    println!("GET /users/1  →  {}", ctrl.handle(1));
    println!("GET /users/2  →  {}", ctrl.handle(2));
    println!("GET /users/99 →  {}", ctrl.handle(99));

    println!("\n✅ Auto-wiring works! Next: v0.3 actual HTTP server.");
}
