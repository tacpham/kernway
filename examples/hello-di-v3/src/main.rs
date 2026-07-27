//! hello-di-v3 — Kernway DI: full auto-wiring (Spring-Boot style)
//!
//! Run: .\kw.ps1 run hello-di-v3   (or: cargo run -p hello-di-v3)
//!
//! Demonstrates everything the container can do:
//!   • register_component in ANY order + refresh() → topological auto-wiring
//!   • inject by interface  (`Arc<dyn Trait>` + `#[provides]`)
//!   • qualifier injection  (`#[inject(qualifier = "...")]`)
//!   • collection injection (`Vec<Arc<dyn Trait>>` — all providers)
//!   • optional injection   (`Option<Arc<T>>` — `None` if absent)
//!   • lifecycle hook       (`#[post_construct(method)]`)
//!   • DiError instead of panics

use di_core::{AppContext, DiError};
use di_macro::Component;
use std::sync::Arc;

// --- Interface injection ----------------------------------------------
// Injectable interfaces MUST declare `Send + Sync` supertraits.

trait UserRepo: Send + Sync {
    fn find(&self, id: u64) -> Option<String>;
}

#[derive(Component)]
#[provides(dyn UserRepo)]
pub struct InMemoryUserRepo;

impl UserRepo for InMemoryUserRepo {
    fn find(&self, id: u64) -> Option<String> {
        match id {
            1 => Some("Alice".into()),
            2 => Some("Bob".into()),
            _ => None,
        }
    }
}

// --- Collection injection: many providers of one interface ------------

trait AuditSink: Send + Sync {
    fn name(&self) -> &'static str;
}

#[derive(Component)]
#[provides(dyn AuditSink)]
pub struct ConsoleAudit;
impl AuditSink for ConsoleAudit {
    fn name(&self) -> &'static str {
        "console"
    }
}

#[derive(Component)]
#[provides(dyn AuditSink)]
pub struct MetricsAudit;
impl AuditSink for MetricsAudit {
    fn name(&self) -> &'static str {
        "metrics"
    }
}

// --- Service: interface + qualifier + collection + optional -----------

#[derive(Component)]
#[post_construct(on_ready)]
pub struct UserService {
    #[inject]
    repo: Arc<dyn UserRepo>, // by interface

    #[inject(qualifier = "app_name")]
    app_name: Arc<String>, // named bean

    #[inject]
    sinks: Vec<Arc<dyn AuditSink>>, // ALL audit sinks

    #[inject]
    feature_flag: Option<Arc<FeatureFlags>>, // optional (not registered here)
}

pub struct FeatureFlags; // exists as a type, but we won't register it

impl UserService {
    /// Lifecycle hook — runs after the whole graph is wired.
    fn on_ready(self: &Arc<Self>, _ctx: &AppContext) -> Result<(), DiError> {
        let sinks: Vec<&str> = self.sinks.iter().map(|s| s.name()).collect();
        println!(
            "🔔 post_construct: UserService ready — sinks={:?}, feature_flags={}",
            sinks,
            if self.feature_flag.is_some() {
                "on"
            } else {
                "absent"
            }
        );
        Ok(())
    }

    fn greet(&self, id: u64) -> String {
        match self.repo.find(id) {
            Some(name) => format!("[{}] Hello, {name}!", self.app_name),
            None => format!("[{}] User #{id} not found", self.app_name),
        }
    }
}

#[derive(Component)]
pub struct UserController {
    #[inject]
    service: Arc<UserService>,
}

fn main() {
    println!("=== Kernway v0.3 — Full DI Demo ===\n");

    let mut ctx = AppContext::new();

    // A qualified config bean supplied up-front.
    ctx.register_qualified::<String>("app_name", Arc::new("kernway".to_string()))
        .unwrap();

    // Register components in DELIBERATELY WRONG order — refresh sorts it out,
    // and builds BOTH audit sinks before the service that collects them.
    ctx.register_component::<UserController>()
        .register_component::<UserService>()
        .register_component::<MetricsAudit>()
        .register_component::<InMemoryUserRepo>()
        .register_component::<ConsoleAudit>();

    match ctx.refresh() {
        Ok(()) => println!("✅ refresh(): {} beans wired\n", ctx.bean_count()),
        Err(e) => {
            eprintln!("❌ DI error: {e}");
            std::process::exit(1);
        }
    }

    let ctrl = ctx.get::<UserController>().unwrap();
    println!("\n--- Simulated requests ---");
    for id in [1, 2, 99] {
        println!("GET /users/{id:<2} → {}", ctrl.service.greet(id));
    }

    println!("\n--- Collection & interface resolution ---");
    println!(
        "get_all_as::<dyn AuditSink>() → {} sinks",
        ctx.get_all_as::<dyn AuditSink>().len()
    );
    println!(
        "get_as::<dyn UserRepo>() → find(1) = {:?}",
        ctx.get_as::<dyn UserRepo>().unwrap().find(1)
    );

    println!(
        "\n✅ Auto-ordering + interface + qualifier + collection + optional + post_construct."
    );
}
