//! Integration tests for `#[derive(Component)]` — exercises the generated
//! `Buildable` / `RegistersComponent` against the real `di_core` runtime.
//!
//! These live here (not in di-core) because the generated code refers to
//! `::di_core::…`, which only resolves when di-core is an *external* crate.

use std::any::Any;
use std::sync::Arc;

use di_core::{AppContext, Container, DiError};
use di_macro::Component;

// --- Basic auto-wiring with out-of-order registration ------------------

#[derive(Component)]
struct Repo;

impl Repo {
    fn name(&self) -> &'static str {
        "repo"
    }
}

#[derive(Component)]
struct Service {
    #[inject]
    repo: Arc<Repo>,
}

#[derive(Component)]
struct Controller {
    #[inject]
    service: Arc<Service>,
}

#[test]
fn refresh_wires_derived_components_in_any_order() {
    let mut ctx = AppContext::new();
    ctx.register_component::<Controller>()
        .register_component::<Service>()
        .register_component::<Repo>();
    ctx.refresh().expect("topo-sort should wire all three");

    let ctrl = ctx.get::<Controller>().unwrap();
    assert_eq!(ctrl.service.repo.name(), "repo");
}

#[test]
fn missing_derived_dependency_is_error_not_panic() {
    let mut ctx = AppContext::new();
    ctx.register_component::<Service>(); // Repo never registered
    let err = ctx.refresh().unwrap_err();
    assert!(matches!(err, DiError::MissingDependency { .. }), "got {err:?}");
}

// --- Interface injection: #[provides(dyn Trait)] + Arc<dyn Trait> ------

trait UserRepo: Send + Sync {
    fn find(&self, id: u64) -> String;
}

#[derive(Component)]
#[provides(dyn UserRepo)]
struct PgUserRepo;

impl UserRepo for PgUserRepo {
    fn find(&self, id: u64) -> String {
        format!("pg#{id}")
    }
}

#[derive(Component)]
struct UserService {
    #[inject]
    repo: Arc<dyn UserRepo>,
}

#[test]
fn trait_object_injection_resolves_concrete_binding() {
    let mut ctx = AppContext::new();
    // Register the consumer first — refresh must still order the provider ahead.
    ctx.register_component::<UserService>()
        .register_component::<PgUserRepo>();
    ctx.refresh().expect("provider must be wired before consumer");

    let svc = ctx.get::<UserService>().unwrap();
    assert_eq!(svc.repo.find(7), "pg#7");

    // The trait binding is independently resolvable.
    let repo = ctx.get_as::<dyn UserRepo>().unwrap();
    assert_eq!(repo.find(1), "pg#1");
}

// --- Qualifier injection: #[inject(qualifier = "...")] -----------------

#[derive(Component)]
struct Config {
    #[inject(qualifier = "db_url")]
    url: Arc<String>,
}

#[test]
fn qualifier_injection_selects_named_bean() {
    let mut ctx = AppContext::new();
    ctx.register_qualified::<String>("db_url", Arc::new("postgres://".to_string()))
        .unwrap();
    ctx.register_qualified::<String>("cache_url", Arc::new("redis://".to_string()))
        .unwrap();

    ctx.register_component::<Config>();
    ctx.refresh().unwrap();

    let cfg = ctx.get::<Config>().unwrap();
    assert_eq!(*cfg.url, "postgres://");
}

// --- Optional injection: #[inject] Option<Arc<T>> ----------------------

#[derive(Component)]
struct WithOptionalDep {
    #[inject]
    repo: Option<Arc<Repo>>,
}

#[test]
fn optional_injection_present_and_absent() {
    // Present.
    let mut ctx = AppContext::new();
    ctx.register_component::<Repo>()
        .register_component::<WithOptionalDep>();
    ctx.refresh().unwrap();
    assert!(ctx.get::<WithOptionalDep>().unwrap().repo.is_some());

    // Absent — no error, resolves to None.
    let mut ctx2 = AppContext::new();
    ctx2.register_component::<WithOptionalDep>();
    ctx2.refresh().expect("optional dep may be missing");
    assert!(ctx2.get::<WithOptionalDep>().unwrap().repo.is_none());
}

// --- Collection injection: #[inject] Vec<Arc<dyn Trait>> ---------------

trait Plugin: Send + Sync {
    fn tag(&self) -> &'static str;
}

#[derive(Component)]
#[provides(dyn Plugin)]
struct PluginA;
impl Plugin for PluginA {
    fn tag(&self) -> &'static str { "A" }
}

#[derive(Component)]
#[provides(dyn Plugin)]
struct PluginB;
impl Plugin for PluginB {
    fn tag(&self) -> &'static str { "B" }
}

#[derive(Component)]
struct PluginRegistry {
    #[inject]
    plugins: Vec<Arc<dyn Plugin>>,
}

#[test]
fn collection_injection_gathers_all_providers() {
    let mut ctx = AppContext::new();
    // Consumer registered first — refresh must build BOTH providers before it.
    ctx.register_component::<PluginRegistry>()
        .register_component::<PluginA>()
        .register_component::<PluginB>();
    ctx.refresh().unwrap();

    let reg = ctx.get::<PluginRegistry>().unwrap();
    let mut tags: Vec<&str> = reg.plugins.iter().map(|p| p.tag()).collect();
    tags.sort_unstable();
    assert_eq!(tags, ["A", "B"], "collection must see the COMPLETE provider set");
}

// --- Lifecycle hook: #[post_construct(method)] -------------------------

use std::sync::atomic::{AtomicBool, Ordering};

static STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Component)]
#[post_construct(start)]
struct Worker;

impl Worker {
    fn start(self: &Arc<Self>, _ctx: &AppContext) -> Result<(), DiError> {
        STARTED.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn post_construct_hook_runs_after_wiring() {
    assert!(!STARTED.load(Ordering::SeqCst));
    let mut ctx = AppContext::new();
    ctx.register_component::<Worker>();
    ctx.refresh().unwrap();
    assert!(STARTED.load(Ordering::SeqCst), "post_construct should have fired");
}

// --- Container seam: build a component against a MOCK, no AppContext ----

/// Minimal mock container that only knows how to hand out one `Repo`.
struct MockContainer {
    repo: Arc<Repo>,
}

impl Container for MockContainer {
    fn get<T: Any + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        (Arc::clone(&self.repo) as Arc<dyn Any + Send + Sync>)
            .downcast::<T>()
            .map_err(|_| DiError::NotFound { type_name: std::any::type_name::<T>() })
    }
    fn get_qualified<T: Any + Send + Sync + 'static>(&self, _q: &str) -> Result<Arc<T>, DiError> {
        Err(DiError::NotFound { type_name: std::any::type_name::<T>() })
    }
    fn get_as<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        Err(DiError::NotFound { type_name: std::any::type_name::<T>() })
    }
    fn get_all<T: Any + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        Vec::new()
    }
    fn get_all_as<T: ?Sized + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        Vec::new()
    }
}

#[test]
fn component_builds_against_a_mock_container() {
    use di_core::Buildable;
    // No AppContext, no refresh — just a hand-rolled container.
    let mock = MockContainer { repo: Arc::new(Repo) };
    let svc = Service::build(&mock).expect("Service should build from the mock");
    assert_eq!(svc.repo.name(), "repo");
}

// --- Bean metadata: #[primary] / #[qualifier] / #[default_impl] --------

trait Notifier: Send + Sync {
    fn channel(&self) -> &'static str;
}

#[derive(Component)]
#[provides(dyn Notifier)]
#[primary]
struct EmailNotifier;
impl Notifier for EmailNotifier {
    fn channel(&self) -> &'static str { "email" }
}

#[derive(Component)]
#[provides(dyn Notifier)]
#[qualifier("sms")]
struct SmsNotifier;
impl Notifier for SmsNotifier {
    fn channel(&self) -> &'static str { "sms" }
}

#[derive(Component)]
struct AlertService {
    #[inject]
    default_channel: Arc<dyn Notifier>,
    #[inject(qualifier = "sms")]
    urgent_channel: Arc<dyn Notifier>,
}

#[test]
fn primary_and_qualifier_disambiguate_trait_bindings() {
    let mut ctx = AppContext::new();
    ctx.register_component::<AlertService>()
        .register_component::<EmailNotifier>()
        .register_component::<SmsNotifier>();
    ctx.refresh().expect("primary + qualifier must resolve both fields");

    let svc = ctx.get::<AlertService>().unwrap();
    // Unqualified `Arc<dyn Notifier>` → the #[primary] provider.
    assert_eq!(svc.default_channel.channel(), "email");
    // `#[inject(qualifier = "sms")]` on a trait field used to be dead code:
    // the binding carried no qualifier, so it always failed with NotFound.
    assert_eq!(svc.urgent_channel.channel(), "sms");

    // The same holds for direct resolution off the context.
    assert_eq!(ctx.get_as::<dyn Notifier>().unwrap().channel(), "email");
    assert_eq!(ctx.get_all_as::<dyn Notifier>().len(), 2);
}

#[test]
fn qualifier_on_the_bean_also_names_the_concrete_type() {
    let mut ctx = AppContext::new();
    ctx.register_component::<SmsNotifier>();
    ctx.refresh().unwrap();
    // `#[qualifier("sms")]` applies to the concrete bean as well as the binding.
    assert!(ctx.get_qualified::<SmsNotifier>("sms").is_ok());
}

// `#[default_impl]` — a framework fallback a user bean silently replaces.

trait Clock: Send + Sync {
    fn now(&self) -> u64;
}

#[derive(Component)]
#[provides(dyn Clock)]
#[default_impl]
struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> u64 { 0 }
}

#[derive(Component)]
#[provides(dyn Clock)]
struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> u64 { 42 }
}

#[test]
fn default_impl_is_used_when_no_user_bean_exists() {
    let mut ctx = AppContext::new();
    ctx.register_component::<SystemClock>();
    ctx.refresh().unwrap();
    assert_eq!(ctx.get_as::<dyn Clock>().unwrap().now(), 0);
}

#[test]
fn user_bean_overrides_default_impl_in_either_order() {
    // Default registered first.
    let mut ctx = AppContext::new();
    ctx.register_component::<SystemClock>()
        .register_component::<FixedClock>();
    ctx.refresh().unwrap();
    assert_eq!(ctx.get_as::<dyn Clock>().unwrap().now(), 42, "user bean must win");
    assert_eq!(ctx.get_all_as::<dyn Clock>().len(), 1, "default must be dropped, not kept");

    // User bean registered first — same outcome.
    let mut ctx2 = AppContext::new();
    ctx2.register_component::<FixedClock>()
        .register_component::<SystemClock>();
    ctx2.refresh().unwrap();
    assert_eq!(ctx2.get_as::<dyn Clock>().unwrap().now(), 42);
}
