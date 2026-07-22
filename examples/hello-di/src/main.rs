//! hello-di — Demo Kernway DI (v0.1)
//!
//! Run: cargo run -p hello-di
//!
//! Demonstrates:
//!   - #[component] marks beans
//!   - #[inject] marks dependencies
//!   - AppContext registers and resolves beans
//!   - User beans override framework defaults

use di_core::AppContext;
use di_macro::component;
use std::sync::Arc;

// --- Bean definitions ---

/// Repository layer — accesses the database (mock).
#[component]
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

/// Service layer — business logic.
/// Note: #[inject] on fields will work in v0.2 (auto-wiring).
/// v0.1 uses manual injection in main().
#[component]
pub struct UserService {
    pub repo: Arc<UserRepository>,
}

impl UserService {
    pub fn get_user_name(&self, id: u64) -> String {
        self.repo
            .find_by_id(id)
            .unwrap_or_else(|| format!("User #{id} not found"))
    }
}

/// Controller layer — HTTP handler (mock).
#[component]
pub struct UserController {
    pub service: Arc<UserService>,
}

impl UserController {
    pub fn handle_get_user(&self, id: u64) -> String {
        format!("HTTP 200: {}", self.service.get_user_name(id))
    }
}

// --- App bootstrap ---

fn main() {
    println!("=== Kernway v0.1 — DI Demo ===\n");

    // --- Step 1: Create AppContext ---
    let mut ctx = AppContext::new();

    // --- Step 2: Register beans (manually in v0.1 — auto-discovery via macros in v0.2) ---

    // Repository (no dependencies)
    ctx.register_instance::<UserRepository>(Arc::new(UserRepository))
        .unwrap();

    // Service (depends on Repository)
    let repo = ctx.get::<UserRepository>().unwrap();
    ctx.register_instance::<UserService>(Arc::new(UserService { repo }))
        .unwrap();

    // Controller (depends on Service)
    let service = ctx.get::<UserService>().unwrap();
    ctx.register_instance::<UserController>(Arc::new(UserController { service }))
        .unwrap();

    println!("✅ {} beans registered", ctx.bean_count());

    // --- Step 3: Use beans ---
    let ctrl = ctx.get::<UserController>().unwrap();

    println!("\n--- Simulated HTTP requests ---");
    println!("GET /users/1  →  {}", ctrl.handle_get_user(1));
    println!("GET /users/2  →  {}", ctrl.handle_get_user(2));
    println!("GET /users/99 →  {}", ctrl.handle_get_user(99));

    println!("\n✅ Done! Next: v0.2 auto-wiring, v0.3 actual HTTP server.");
}
