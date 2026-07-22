//! AppContext — DI container.
//!
//! Equivalent to `ApplicationContext` in Spring.
//! Stores all beans and resolves dependencies.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use crate::bean::{BeanEntry, BeanOrigin};
use crate::buildable::RegistersComponent;
use crate::error::DiError;

/// Bean instance — Arc<dyn Any + Send + Sync>.
type BeanInstance = Arc<dyn Any + Send + Sync>;

/// A `TypeId` is already a well-distributed hash, so running SipHash over its
/// bytes on every `get` is pure overhead. This passthrough `Hasher` captures the
/// value `TypeId` feeds in and returns it directly — a ~2× win on the hot path,
/// with zero extra dependencies (pure `std`).
#[derive(Default)]
struct TypeIdHasher {
    hash: u64,
}

impl Hasher for TypeIdHasher {
    fn finish(&self) -> u64 {
        self.hash
    }
    fn write_u64(&mut self, i: u64) {
        self.hash = i;
    }
    fn write_u128(&mut self, i: u128) {
        // `TypeId` is a u128 internally on current Rust; fold to 64 bits.
        self.hash = (i as u64) ^ ((i >> 64) as u64);
    }
    // Fallback for any other feeding shape — fold bytes (TypeId is uniform, so
    // this stays collision-light).
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash = self.hash.rotate_left(8) ^ (b as u64);
        }
    }
}

/// `HashMap` keyed by `TypeId` with the passthrough hasher.
type TypeIdMap<V> = HashMap<TypeId, V, BuildHasherDefault<TypeIdHasher>>;

/// Builds a component, registers its instance, and publishes trait bindings.
/// `Send + Sync` keeps `AppContext` shareable as `Arc<AppContext>` across threads.
type BuildFn = Box<dyn FnOnce(&mut AppContext) -> Result<(), DiError> + Send + Sync>;

/// A component queued for topological auto-wiring by [`AppContext::refresh`].
///
/// Produced by [`AppContext::register_component`]; consumed (in dependency order)
/// by `refresh`.
struct ComponentDef {
    type_name: &'static str,
    /// TypeIds that must be resolved before this bean can be built.
    deps:      Vec<TypeId>,
    /// Soft deps (`Option`/`Vec` fields): order-if-present, never required.
    opt_deps:  Vec<TypeId>,
    /// TypeIds this bean satisfies once built (own type + `#[provides]` traits).
    provides:  Vec<TypeId>,
    /// Builds the bean, registers the instance, and publishes trait bindings.
    /// The generated closures capture nothing, so they are `Send + Sync`.
    build:     BuildFn,
}

/// DI container.
///
/// # Example
///
/// ```rust
/// use di_core::AppContext;
/// use std::sync::Arc;
///
/// let mut ctx = AppContext::new();
///
/// // Register a bean manually
/// ctx.register_instance::<String>(Arc::new("hello".to_string())).unwrap();
///
/// // Retrieve the bean
/// let s: Arc<String> = ctx.get::<String>().unwrap();
/// assert_eq!(*s, "hello");
/// ```
pub struct AppContext {
    /// Bean instances, keyed by TypeId.
    ///
    /// Trait bindings (from `register_as`) live here too, keyed by
    /// `TypeId::of::<Arc<dyn Trait>>()` with an `Arc<Arc<dyn Trait>>` value.
    instances: TypeIdMap<Vec<(BeanEntry, BeanInstance)>>,
    /// Components awaiting topological wiring by [`AppContext::refresh`].
    pending: Vec<ComponentDef>,
}

impl AppContext {
    /// Create an empty context.
    pub fn new() -> Self {
        Self {
            instances: TypeIdMap::default(),
            pending: Vec::new(),
        }
    }

    /// Register a bean instance in the context.
    pub fn register_instance<T: Any + Send + Sync + 'static>(
        &mut self,
        instance: Arc<T>,
    ) -> Result<(), DiError> {
        self.register_with_entry(
            BeanEntry::new(TypeId::of::<T>(), std::any::type_name::<T>(), BeanOrigin::User),
            instance as BeanInstance,
        )
    }

    /// Register with complete metadata (used by di-macro).
    ///
    /// Validates that `instance`'s concrete type matches `entry.type_id` — this is
    /// what makes the downcast in [`get`](Self::get) infallible. Returns
    /// [`DiError::TypeMismatch`] otherwise (instead of a later hot-path panic).
    pub fn register_with_entry(
        &mut self,
        entry: BeanEntry,
        instance: BeanInstance,
    ) -> Result<(), DiError> {
        let type_id = entry.type_id;

        // The stored value is `Arc<dyn Any>`; its inner concrete type must equal
        // the key we file it under, or `get` would downcast to the wrong type.
        if (*instance).type_id() != type_id {
            return Err(DiError::TypeMismatch { type_name: entry.type_name });
        }

        let list = self.instances.entry(type_id).or_default();

        // If a user bean exists, ignore the framework default
        if entry.is_default() {
            if list.iter().any(|(e, _)| !e.is_default()) {
                // User bean exists → skip the default
                return Ok(());
            }
        } else {
            // User bean → remove all framework defaults
            list.retain(|(e, _)| !e.is_default());
        }

        list.push((entry, instance));
        Ok(())
    }

    /// Get a bean by type.
    /// Returns `DiError::NotFound` if none exists.
    /// Returns `DiError::Ambiguous` if multiple beans exist and none is primary.
    pub fn get<T: Any + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        let type_id = TypeId::of::<T>();
        let type_name = std::any::type_name::<T>();

        let list = self
            .instances
            .get(&type_id)
            .ok_or(DiError::NotFound { type_name })?;

        let instance = if list.len() == 1 {
            &list[0].1
        } else {
            // More than one → exactly one must be `#[primary]`. Two primaries is
            // as ambiguous as none, so report it instead of picking the first.
            let mut primaries = list.iter().filter(|(e, _)| e.is_primary);
            let (_, instance) = primaries.next().ok_or(DiError::Ambiguous { type_name })?;
            if primaries.next().is_some() {
                return Err(DiError::Ambiguous { type_name });
            }
            instance
        };

        // Infallible: `register_with_entry` guarantees the stored concrete type
        // matches this `TypeId`.
        Ok(Arc::clone(instance)
            .downcast::<T>()
            .expect("AppContext invariant: registered type matches its TypeId key"))
    }

    /// Get a bean by qualifier.
    pub fn get_qualified<T: Any + Send + Sync + 'static>(
        &self,
        qualifier: &str,
    ) -> Result<Arc<T>, DiError> {
        let type_id = TypeId::of::<T>();
        let type_name = std::any::type_name::<T>();

        let list = self
            .instances
            .get(&type_id)
            .ok_or(DiError::NotFound { type_name })?;

        // Detect duplicate qualifiers instead of silently returning the first
        // match: two beans sharing a qualifier is a configuration error.
        let mut matches = list.iter().filter(|(e, _)| e.qualifier == Some(qualifier));
        let (_, instance) = matches.next().ok_or(DiError::NotFound { type_name })?;
        if matches.next().is_some() {
            return Err(DiError::Ambiguous { type_name });
        }

        Ok(Arc::clone(instance)
            .downcast::<T>()
            .expect("AppContext invariant: registered type matches its TypeId key"))
    }

    /// All beans registered for a concrete type (for `#[inject] Vec<Arc<T>>`).
    ///
    /// Returns an empty `Vec` if none — never errors. Order is registration order.
    pub fn get_all<T: Any + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        self.instances
            .get(&TypeId::of::<T>())
            .map(|list| {
                list.iter()
                    .filter_map(|(_, inst)| Arc::clone(inst).downcast::<T>().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// All beans bound to a trait (for `#[inject] Vec<Arc<dyn Trait>>`).
    ///
    /// Returns an empty `Vec` if none — never errors.
    pub fn get_all_as<T: ?Sized + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        self.instances
            .get(&TypeId::of::<Arc<T>>())
            .map(|list| {
                list.iter()
                    .filter_map(|(_, inst)| {
                        Arc::clone(inst)
                            .downcast::<Arc<T>>()
                            .ok()
                            .map(|arc_arc| (*arc_arc).clone())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Number of registered beans.
    pub fn bean_count(&self) -> usize {
        self.instances.values().map(|v| v.len()).sum()
    }

    /// Build and register a bean automatically via the `Buildable` trait.
    ///
    /// Calls `T::build(ctx)` to create an instance (dependencies must be built first).
    /// Then automatically registers it in the context.
    ///
    /// # Example
    /// ```rust,ignore
    /// ctx.build::<UserRepository>()?;   // no deps → build first
    /// ctx.build::<UserService>()?;      // dep UserRepository already present → Ok
    /// ctx.build::<UserController>()?;   // dep UserService already present → Ok
    /// ```
    pub fn build<T>(&mut self) -> Result<Arc<T>, DiError>
    where
        T: crate::buildable::Buildable + Any,
    {
        let instance = T::build(self)?;
        self.register_instance::<T>(Arc::clone(&instance))?;
        Ok(instance)
    }

    // ------------------------------------------------------------------
    // Auto-wiring: declarative registration + topological refresh
    // (equivalent to Spring's ApplicationContext.refresh()).
    // ------------------------------------------------------------------

    /// Queue a component for auto-wiring — order-independent.
    ///
    /// Unlike [`build`](Self::build), you may register components in **any**
    /// order; [`refresh`](Self::refresh) resolves the dependency graph and
    /// builds them in topological order. Chainable.
    ///
    /// # Example
    /// ```rust,ignore
    /// ctx.register_component::<UserController>()   // depends on UserService
    ///    .register_component::<UserService>()      // depends on UserRepository
    ///    .register_component::<UserRepository>();  // no deps
    /// ctx.refresh()?;                               // wires all three correctly
    /// ```
    pub fn register_component<T: RegistersComponent>(&mut self) -> &mut Self {
        self.pending.push(ComponentDef {
            type_name: std::any::type_name::<T>(),
            deps:      T::dependencies(),
            opt_deps:  T::optional_dependencies(),
            provides:  T::provides(),
            build: Box::new(|ctx: &mut AppContext| {
                let instance = T::build(ctx)?;
                // Honour the bean's own metadata (`#[default_impl]`, `#[primary]`,
                // `#[qualifier]`) rather than filing everything as a plain user bean.
                ctx.register_with_entry(
                    Self::component_entry::<T>(TypeId::of::<T>(), std::any::type_name::<T>()),
                    Arc::clone(&instance) as BeanInstance,
                )?;
                T::register_bindings(ctx, &instance)?;
                T::post_construct(ctx, &instance)?;
                Ok(())
            }),
        });
        self
    }

    /// Bean entry carrying `T`'s declared metadata (`#[default_impl]`,
    /// `#[primary]`, `#[qualifier]`). Shared by the concrete registration and by
    /// the `#[provides]` trait bindings so both resolve identically.
    fn component_entry<T: RegistersComponent>(
        type_id: TypeId,
        type_name: &'static str,
    ) -> BeanEntry {
        let mut entry = BeanEntry::new(type_id, type_name, T::bean_origin());
        if T::is_primary() {
            entry = entry.primary();
        }
        if let Some(q) = T::qualifier() {
            entry = entry.qualifier(q);
        }
        entry
    }

    /// Build every component queued with [`register_component`](Self::register_component),
    /// in dependency order.
    ///
    /// Uses Kahn's algorithm over the declared dependency graph. Beans already
    /// present (via [`register_instance`](Self::register_instance) /
    /// [`register_as`](Self::register_as)) count as satisfied dependencies.
    ///
    /// # Errors
    /// - [`DiError::CircularDependency`] when the remaining components form a cycle.
    /// - [`DiError::MissingDependency`] when a component needs a type nobody provides.
    pub fn refresh(&mut self) -> Result<(), DiError> {
        // Loop so components registered *during* a build (re-entrancy) are
        // picked up by a subsequent pass.
        while !self.pending.is_empty() {
            let defs = std::mem::take(&mut self.pending);

            // Dependencies already satisfied by pre-registered beans/bindings.
            let mut available: HashSet<TypeId> = self.instances.keys().copied().collect();
            // Count of not-yet-built batch providers per TypeId. A *hard* dep needs
            // one provider (→ `available`); a *soft* dep (Option/Vec) waits for ALL
            // batch providers so a `Vec` sees the complete set.
            let mut pending_providers: HashMap<TypeId, usize> = HashMap::new();
            for def in &defs {
                for p in &def.provides {
                    *pending_providers.entry(*p).or_insert(0) += 1;
                }
            }
            // TypeIds provided somewhere in this batch (for cycle vs missing).
            let provided_by_batch: HashSet<TypeId> = pending_providers.keys().copied().collect();

            let mut remaining: Vec<Option<ComponentDef>> = defs.into_iter().map(Some).collect();

            // Repeatedly build any component whose deps are all available.
            let mut built_any = true;
            while built_any {
                built_any = false;
                for slot in remaining.iter_mut() {
                    let ready = matches!(slot, Some(def)
                        // Hard deps: at least one provider built.
                        if def.deps.iter().all(|d| available.contains(d))
                        // Soft deps: all batch providers built (0 remaining).
                        && def.opt_deps.iter().all(|d| {
                            pending_providers.get(d).copied().unwrap_or(0) == 0
                        }));
                    if ready {
                        let def = slot.take().expect("slot checked ready");
                        let provides = def.provides.clone();
                        let build = def.build;
                        build(self)?;
                        for p in provides {
                            available.insert(p);
                            if let Some(c) = pending_providers.get_mut(&p) {
                                *c = c.saturating_sub(1);
                            }
                        }
                        built_any = true;
                    }
                }
            }

            // Anything left stalled on a *hard* dep is an error; a stall on only
            // soft (Option/Vec) deps means a mutual-optional cycle — build anyway.
            if remaining.iter().any(|s| s.is_some()) {
                // 1) A hard dep nobody provides → missing.
                for def in remaining.iter().flatten() {
                    for dep in &def.deps {
                        if !available.contains(dep) && !provided_by_batch.contains(dep) {
                            return Err(DiError::MissingDependency { type_name: def.type_name });
                        }
                    }
                }
                // 2) A hard dep still unavailable (but provided in-batch) → real cycle.
                let hard_cycle = remaining
                    .iter()
                    .flatten()
                    .any(|def| def.deps.iter().any(|d| !available.contains(d)));
                if hard_cycle {
                    // Report only the components still blocked on an unmet hard dep
                    // (i.e. the ones actually in the cycle), as a set — not a fake path.
                    let cycle = remaining
                        .iter()
                        .flatten()
                        .filter(|def| def.deps.iter().any(|d| !available.contains(d)))
                        .map(|d| d.type_name)
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(DiError::CircularDependency { cycle });
                }
                // 3) Soft-only stall: build the rest, ignoring soft ordering.
                for slot in remaining.iter_mut() {
                    if let Some(def) = slot.take() {
                        let provides = def.provides.clone();
                        (def.build)(self)?;
                        available.extend(provides);
                    }
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Interface (trait-object) beans.
    // ------------------------------------------------------------------

    /// Register a concrete instance under a trait so it can be injected as
    /// `Arc<dyn Trait>`.
    ///
    /// The interface must declare `Send + Sync` supertraits
    /// (`trait UserRepo: Send + Sync { ... }`) — otherwise `dyn UserRepo` is
    /// not `Send + Sync` and this will not compile.
    ///
    /// Stored in the same map as concrete beans, keyed by `TypeId::of::<Arc<T>>()`.
    ///
    /// Registered as a plain `User` bean (no primary / qualifier). If two
    /// providers back the same trait, [`get_as`](Self::get_as) reports
    /// [`DiError::Ambiguous`]; collect them with [`get_all_as`](Self::get_all_as)
    /// instead, or use
    /// [`register_as_component`](Self::register_as_component) — what
    /// `#[provides]` emits — to carry `#[primary]` / `#[qualifier]` onto the
    /// binding.
    pub fn register_as<T: ?Sized + Send + Sync + 'static>(
        &mut self,
        instance: Arc<T>,
    ) -> Result<(), DiError> {
        let erased: BeanInstance = Arc::new(instance); // Arc<Arc<T>>
        self.register_with_entry(
            BeanEntry::new(
                TypeId::of::<Arc<T>>(),
                std::any::type_name::<Arc<T>>(),
                BeanOrigin::User,
            ),
            erased,
        )
    }

    /// Register a trait binding carrying the metadata of the component that
    /// provides it — used by `#[derive(Component)]` + `#[provides(dyn Trait)]`.
    ///
    /// This is what makes `#[primary]` / `#[qualifier("…")]` work for
    /// `Arc<dyn Trait>` injection: the binding is filed under the *same*
    /// qualifier as the concrete bean, so `#[inject(qualifier = "sql")]` on an
    /// `Arc<dyn UserRepo>` field resolves the provider named `"sql"`.
    pub fn register_as_component<T, Tr>(&mut self, instance: Arc<Tr>) -> Result<(), DiError>
    where
        T: RegistersComponent,
        Tr: ?Sized + Send + Sync + 'static,
    {
        let erased: BeanInstance = Arc::new(instance); // Arc<Arc<Tr>>
        self.register_with_entry(
            Self::component_entry::<T>(
                TypeId::of::<Arc<Tr>>(),
                std::any::type_name::<Arc<Tr>>(),
            ),
            erased,
        )
    }

    /// Resolve a trait-object bean registered via [`register_as`](Self::register_as).
    pub fn get_as<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        let arc_arc: Arc<Arc<T>> = self.get::<Arc<T>>()?;
        // Deref the outer Arc to the stored `Arc<T>` and clone that.
        Ok((*arc_arc).clone())
    }

    /// Register a bean under a qualifier (name) — resolved by
    /// [`get_qualified`](Self::get_qualified) or `#[inject(qualifier = "...")]`.
    pub fn register_qualified<T: Any + Send + Sync + 'static>(
        &mut self,
        qualifier: &'static str,
        instance: Arc<T>,
    ) -> Result<(), DiError> {
        self.register_with_entry(
            BeanEntry::new(
                TypeId::of::<T>(),
                std::any::type_name::<T>(),
                BeanOrigin::User,
            )
            .qualifier(qualifier),
            instance as BeanInstance,
        )
    }
}

/// `AppContext` is the canonical [`Container`]. Methods delegate to the inherent
/// ones via explicit `AppContext::…` paths (inherent, never the trait) so there is
/// no risk of accidental self-recursion.
impl crate::container::Container for AppContext {
    fn get<T: Any + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        AppContext::get::<T>(self)
    }
    fn get_qualified<T: Any + Send + Sync + 'static>(
        &self,
        qualifier: &str,
    ) -> Result<Arc<T>, DiError> {
        AppContext::get_qualified::<T>(self, qualifier)
    }
    fn get_as<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        AppContext::get_as::<T>(self)
    }
    fn get_all<T: Any + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        AppContext::get_all::<T>(self)
    }
    fn get_all_as<T: ?Sized + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        AppContext::get_all_as::<T>(self)
    }
}

impl Default for AppContext {
    fn default() -> Self {
        Self::new()
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_get_simple_bean() {
        let mut ctx = AppContext::new();
        ctx.register_instance::<String>(Arc::new("hello".to_string()))
            .unwrap();
        let s = ctx.get::<String>().unwrap();
        assert_eq!(*s, "hello");
    }

    #[test]
    fn not_found_returns_error() {
        let ctx = AppContext::new();
        let result = ctx.get::<String>();
        assert!(matches!(result, Err(DiError::NotFound { .. })));
    }

    #[test]
    fn framework_default_replaced_by_user_bean() {
        let mut ctx = AppContext::new();

        // Register the framework default
        ctx.register_with_entry(
            BeanEntry::new(TypeId::of::<String>(), "String", BeanOrigin::FrameworkDefault),
            Arc::new("default".to_string()) as BeanInstance,
        )
        .unwrap();

        // Register the user bean — it must take precedence
        ctx.register_with_entry(
            BeanEntry::new(TypeId::of::<String>(), "String", BeanOrigin::User),
            Arc::new("user".to_string()) as BeanInstance,
        )
        .unwrap();

        let s = ctx.get::<String>().unwrap();
        assert_eq!(*s, "user");
    }

    #[test]
    fn ambiguous_without_primary_returns_error() {
        let mut ctx = AppContext::new();
        ctx.register_with_entry(
            BeanEntry::new(TypeId::of::<String>(), "String", BeanOrigin::User),
            Arc::new("a".to_string()) as BeanInstance,
        )
        .unwrap();
        ctx.register_with_entry(
            BeanEntry::new(TypeId::of::<String>(), "String", BeanOrigin::User),
            Arc::new("b".to_string()) as BeanInstance,
        )
        .unwrap();
        assert!(matches!(ctx.get::<String>(), Err(DiError::Ambiguous { .. })));
    }

    #[test]
    fn single_primary_wins_over_the_others() {
        let mut ctx = AppContext::new();
        ctx.register_instance::<String>(Arc::new("plain".to_string())).unwrap();
        ctx.register_with_entry(
            BeanEntry::new(TypeId::of::<String>(), "String", BeanOrigin::User).primary(),
            Arc::new("chosen".to_string()) as BeanInstance,
        )
        .unwrap();
        assert_eq!(*ctx.get::<String>().unwrap(), "chosen");
    }

    #[test]
    fn two_primaries_are_ambiguous_not_first_wins() {
        let mut ctx = AppContext::new();
        for s in ["a", "b"] {
            ctx.register_with_entry(
                BeanEntry::new(TypeId::of::<String>(), "String", BeanOrigin::User).primary(),
                Arc::new(s.to_string()) as BeanInstance,
            )
            .unwrap();
        }
        assert!(matches!(ctx.get::<String>(), Err(DiError::Ambiguous { .. })));
    }

    #[test]
    fn bean_count_increments() {
        let mut ctx = AppContext::new();
        assert_eq!(ctx.bean_count(), 0);
        ctx.register_instance::<String>(Arc::new("x".to_string())).unwrap();
        assert_eq!(ctx.bean_count(), 1);
        ctx.register_instance::<u64>(Arc::new(42u64)).unwrap();
        assert_eq!(ctx.bean_count(), 2);
    }

    #[test]
    fn get_qualified_returns_correct_bean() {
        let mut ctx = AppContext::new();
        let entry = BeanEntry::new(TypeId::of::<String>(), "String", BeanOrigin::User)
            .qualifier("primary");
        ctx.register_with_entry(entry, Arc::new("primary-bean".to_string()) as BeanInstance)
            .unwrap();

        let s = ctx.get_qualified::<String>("primary").unwrap();
        assert_eq!(*s, "primary-bean");
    }

    #[test]
    fn get_qualified_missing_qualifier_returns_error() {
        let mut ctx = AppContext::new();
        ctx.register_instance::<String>(Arc::new("noqual".to_string())).unwrap();
        assert!(ctx.get_qualified::<String>("missing").is_err());
    }

    #[test]
    fn get_qualified_duplicate_qualifier_is_ambiguous() {
        // Two beans sharing a qualifier must error, not silently pick the first.
        let mut ctx = AppContext::new();
        ctx.register_qualified::<String>("dup", Arc::new("first".to_string())).unwrap();
        ctx.register_qualified::<String>("dup", Arc::new("second".to_string())).unwrap();
        assert!(matches!(
            ctx.get_qualified::<String>("dup"),
            Err(DiError::Ambiguous { .. })
        ));
    }

    // --- Auto-wiring: register_component + refresh -------------------

    use crate::buildable::{Buildable, RegistersComponent};
    use crate::container::Container;

    struct Repo;
    struct Service {
        repo: Arc<Repo>,
    }
    struct Controller {
        service: Arc<Service>,
    }

    impl Buildable for Repo {
        fn build<C: Container + ?Sized>(_ctx: &C) -> Result<Arc<Self>, DiError> {
            Ok(Arc::new(Repo))
        }
    }
    impl RegistersComponent for Repo {}

    impl Buildable for Service {
        fn build<C: Container + ?Sized>(ctx: &C) -> Result<Arc<Self>, DiError> {
            Ok(Arc::new(Service { repo: ctx.get::<Repo>()? }))
        }
    }
    impl RegistersComponent for Service {
        fn dependencies() -> Vec<TypeId> {
            vec![TypeId::of::<Repo>()]
        }
    }

    impl Buildable for Controller {
        fn build<C: Container + ?Sized>(ctx: &C) -> Result<Arc<Self>, DiError> {
            Ok(Arc::new(Controller { service: ctx.get::<Service>()? }))
        }
    }
    impl RegistersComponent for Controller {
        fn dependencies() -> Vec<TypeId> {
            vec![TypeId::of::<Service>()]
        }
    }

    #[test]
    fn refresh_wires_in_dependency_order_despite_reverse_registration() {
        let mut ctx = AppContext::new();
        // Register in the WRONG order on purpose.
        ctx.register_component::<Controller>()
            .register_component::<Service>()
            .register_component::<Repo>();
        ctx.refresh().expect("should topo-sort and wire all three");

        assert_eq!(ctx.bean_count(), 3);
        // Controller was built => its whole inject chain resolved.
        let controller = ctx.get::<Controller>().unwrap();
        // Same Repo instance flows Repo -> Service -> Controller.
        assert!(Arc::ptr_eq(&controller.service.repo, &ctx.get::<Repo>().unwrap()));
    }

    #[test]
    fn refresh_uses_preregistered_instance_as_satisfied_dep() {
        let mut ctx = AppContext::new();
        ctx.register_instance::<Repo>(Arc::new(Repo)).unwrap(); // pre-registered dep
        ctx.register_component::<Service>();
        ctx.refresh().expect("Service dep satisfied by pre-registered Repo");
        let _ = ctx.get::<Service>().unwrap();
    }

    #[test]
    fn refresh_missing_dependency_reports_error() {
        let mut ctx = AppContext::new();
        ctx.register_component::<Service>(); // Repo never provided
        let err = ctx.refresh().unwrap_err();
        assert!(matches!(err, DiError::MissingDependency { .. }), "got {err:?}");
    }

    // Cyclic pair: X depends on Y, Y depends on X.
    struct X;
    struct Y;
    impl Buildable for X {
        fn build<C: Container + ?Sized>(_c: &C) -> Result<Arc<Self>, DiError> { Ok(Arc::new(X)) }
    }
    impl RegistersComponent for X {
        fn dependencies() -> Vec<TypeId> { vec![TypeId::of::<Y>()] }
    }
    impl Buildable for Y {
        fn build<C: Container + ?Sized>(_c: &C) -> Result<Arc<Self>, DiError> { Ok(Arc::new(Y)) }
    }
    impl RegistersComponent for Y {
        fn dependencies() -> Vec<TypeId> { vec![TypeId::of::<X>()] }
    }

    #[test]
    fn refresh_detects_circular_dependency() {
        let mut ctx = AppContext::new();
        ctx.register_component::<X>().register_component::<Y>();
        let err = ctx.refresh().unwrap_err();
        assert!(matches!(err, DiError::CircularDependency { .. }), "got {err:?}");
    }

    // --- Interface (trait-object) beans -----------------------------

    trait Greeter: Send + Sync {
        fn greet(&self) -> String;
    }
    struct English;
    impl Greeter for English {
        fn greet(&self) -> String { "hello".into() }
    }

    #[test]
    fn register_as_and_get_as_round_trip() {
        let mut ctx = AppContext::new();
        ctx.register_as::<dyn Greeter>(Arc::new(English)).unwrap();
        let g: Arc<dyn Greeter> = ctx.get_as::<dyn Greeter>().unwrap();
        assert_eq!(g.greet(), "hello");
    }

    #[test]
    fn get_as_missing_returns_error() {
        let ctx = AppContext::new();
        assert!(matches!(ctx.get_as::<dyn Greeter>(), Err(DiError::NotFound { .. })));
    }

    #[test]
    fn register_qualified_resolves_by_name() {
        let mut ctx = AppContext::new();
        ctx.register_qualified::<String>("primary", Arc::new("A".to_string())).unwrap();
        ctx.register_qualified::<String>("backup", Arc::new("B".to_string())).unwrap();
        assert_eq!(*ctx.get_qualified::<String>("primary").unwrap(), "A");
        assert_eq!(*ctx.get_qualified::<String>("backup").unwrap(), "B");
    }

    // --- Collection resolution (get_all / get_all_as) ---------------

    #[test]
    fn get_all_returns_every_bean_of_a_type() {
        let mut ctx = AppContext::new();
        assert!(ctx.get_all::<String>().is_empty());
        ctx.register_qualified::<String>("a", Arc::new("x".into())).unwrap();
        ctx.register_qualified::<String>("b", Arc::new("y".into())).unwrap();
        let all = ctx.get_all::<String>();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn get_all_as_returns_every_trait_binding() {
        let mut ctx = AppContext::new();
        ctx.register_as::<dyn Greeter>(Arc::new(English)).unwrap();
        ctx.register_as::<dyn Greeter>(Arc::new(English)).unwrap();
        let greeters = ctx.get_all_as::<dyn Greeter>();
        assert_eq!(greeters.len(), 2);
        assert_eq!(greeters[0].greet(), "hello");
    }

    // --- Robustness: register_with_entry validates the type ----------

    #[test]
    fn register_with_entry_rejects_mismatched_type_instead_of_panicking() {
        let mut ctx = AppContext::new();
        // Pair a String TypeId with a u64 instance — the old code would panic
        // later in `get`; now it must be rejected up-front.
        let bad = ctx.register_with_entry(
            BeanEntry::new(TypeId::of::<String>(), "String", BeanOrigin::User),
            Arc::new(42u64) as BeanInstance,
        );
        assert!(matches!(bad, Err(DiError::TypeMismatch { .. })), "got {bad:?}");
        // And `get::<String>()` does not find a corrupt entry → clean NotFound.
        assert!(matches!(ctx.get::<String>(), Err(DiError::NotFound { .. })));
    }

    #[test]
    fn register_with_entry_accepts_matching_type() {
        let mut ctx = AppContext::new();
        ctx.register_with_entry(
            BeanEntry::new(TypeId::of::<u64>(), "u64", BeanOrigin::User),
            Arc::new(7u64) as BeanInstance,
        )
        .unwrap();
        assert_eq!(*ctx.get::<u64>().unwrap(), 7);
    }

    #[test]
    fn passthrough_hasher_lookups_work_across_many_types() {
        // Exercises the custom TypeId hasher through real inserts/lookups.
        let mut ctx = AppContext::new();
        ctx.register_instance::<u8>(Arc::new(1u8)).unwrap();
        ctx.register_instance::<u16>(Arc::new(2u16)).unwrap();
        ctx.register_instance::<u32>(Arc::new(3u32)).unwrap();
        ctx.register_instance::<String>(Arc::new("s".to_string())).unwrap();
        assert_eq!(*ctx.get::<u8>().unwrap(), 1);
        assert_eq!(*ctx.get::<u16>().unwrap(), 2);
        assert_eq!(*ctx.get::<u32>().unwrap(), 3);
        assert_eq!(*ctx.get::<String>().unwrap(), "s");
    }
}