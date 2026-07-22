//! Buildable trait — automatic construction from AppContext.

use crate::container::Container;
use crate::context::AppContext;
use crate::error::DiError;
use std::any::TypeId;
use std::sync::Arc;

/// Trait for beans that can construct themselves from AppContext.
///
/// Generated automatically by `#[derive(Component)]` — users should not implement it manually.
///
/// # Example (generated code)
/// ```rust,ignore
/// // User writes:
/// #[derive(Component)]
/// pub struct UserService {
///     #[inject]
///     repo: Arc<UserRepository>,
/// }
///
/// // Macro generate:
/// impl Buildable for UserService {
///     fn build<C: Container>(ctx: &C) -> Result<Arc<Self>, DiError> {
///         Ok(Arc::new(UserService {
///             repo: ctx.get::<UserRepository>()?,
///         }))
///     }
/// }
/// ```
pub trait Buildable: Sized + Send + Sync + 'static {
    /// Create a new instance, resolving all `#[inject]` dependencies from any
    /// [`Container`] (the real `AppContext`, a child context, or a test mock).
    ///
    /// Returns `Err(DiError)` instead of panicking when a dependency is missing.
    fn build<C: Container + ?Sized>(ctx: &C) -> Result<Arc<Self>, DiError>;
}

/// Metadata + binding registration for a `#[derive(Component)]` bean.
///
/// Generated automatically alongside [`Buildable`]. Drives the topological
/// auto-wiring performed by [`AppContext::refresh`](crate::AppContext::refresh):
/// `dependencies()` describes the incoming edges, `provides()` the outgoing ones
/// (the bean's own type plus any `#[provides(dyn Trait)]` bindings), and
/// `register_bindings` publishes those trait bindings after the concrete bean
/// is built.
pub trait RegistersComponent: Buildable {
    /// TypeIds this bean needs resolved before it can be built.
    ///
    /// Concrete deps are keyed by `TypeId::of::<C>()`; trait deps (`Arc<dyn Tr>`)
    /// by `TypeId::of::<Arc<dyn Tr>>()`.
    fn dependencies() -> Vec<TypeId> {
        Vec::new()
    }

    /// Soft dependencies from `Option<Arc<T>>` / `Vec<Arc<T>>` fields.
    ///
    /// `refresh` orders a provider ahead of this bean *if* one is registered,
    /// but — unlike [`dependencies`](Self::dependencies) — a missing one is not
    /// an error (resolves to `None` / an empty `Vec`).
    fn optional_dependencies() -> Vec<TypeId> {
        Vec::new()
    }

    /// TypeIds this bean satisfies once built (its own type + `#[provides]` traits).
    fn provides() -> Vec<TypeId> {
        vec![TypeId::of::<Self>()]
    }

    /// Publish any `#[provides(dyn Trait)]` bindings for an already-built instance.
    fn register_bindings(_ctx: &mut AppContext, _this: &Arc<Self>) -> Result<(), DiError> {
        Ok(())
    }

    /// Post-construction hook — runs after the bean is built, registered, and its
    /// bindings published, in dependency order (Spring's `@PostConstruct`).
    ///
    /// Unlike `build`, it receives `Arc<Self>`, so it can register itself as a
    /// listener, spawn a worker holding `Arc<Self>`, warm a cache, etc. Wired by
    /// `#[post_construct(method)]`; default is a no-op.
    fn post_construct(_ctx: &AppContext, _this: &Arc<Self>) -> Result<(), DiError> {
        Ok(())
    }
}
