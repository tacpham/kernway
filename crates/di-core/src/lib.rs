//! # di-core
//!
//! Dependency Injection runtime for Kernway — the equivalent of Spring's
//! `ApplicationContext`, minus the reflection.
//!
//! ## The idea
//!
//! Spring resolves the dependency graph at runtime by reading annotations off
//! loaded classes. Rust has no runtime reflection, so di-core splits the job in
//! two: the [`di-macro`] derive reads your struct at **compile time** and emits
//! the wiring code, and this crate stores the resulting beans and hands them
//! back on demand.
//!
//! The consequence worth knowing: a missing `#[inject]` dependency is a build
//! failure in the generated code, not a 3am `NoSuchBeanDefinitionException`.
//! What is left at runtime is a map lookup.
//!
//! [`di-macro`]: https://docs.rs/di-macro
//!
//! ## How a bean gets into the container
//!
//! Two routes, and they meet in the same map.
//!
//! **Direct** — you hand over an already-built value:
//!
//! ```text
//! register_instance::<T>(value)  ─────────────►  instances: HashMap<TypeId, Arc<dyn Any>>
//! ```
//!
//! **Auto-wired** — you declare the type and let the container order the work:
//!
//! ```text
//! register_component::<T>()  ──►  pending: Vec<ComponentDef>
//!                                       │  (declared deps + provides, no building yet)
//!                                       ▼
//!                                 refresh()
//!                                       │  Kahn's algorithm over the declared graph
//!                                       ▼
//!               T::build(ctx) → register → T::register_bindings → T::post_construct
//! ```
//!
//! Because `refresh` sorts topologically, [`register_component`] calls are
//! **order-independent** — register a controller before the service it needs and
//! it still works. That is the whole point of the two-phase design: declaration
//! is cheap and unordered, construction is ordered and happens once.
//!
//! [`register_component`]: AppContext::register_component
//!
//! ## The refresh algorithm, concretely
//!
//! `refresh` loops until nothing is pending. Each pass:
//!
//! 1. Seeds `available` with the `TypeId`s already in the container.
//! 2. Repeatedly builds every component whose dependencies are all available,
//!    until a full sweep builds nothing.
//! 3. Classifies whatever is left over:
//!    - a hard dep nobody in the batch provides → [`DiError::MissingDependency`]
//!    - a hard dep provided in-batch but never satisfied → [`DiError::CircularDependency`]
//!    - stalled on **soft** deps only (`Option<Arc<T>>` / `Vec<Arc<T>>`) → not an
//!      error; build the rest and let the optionals resolve to `None` / empty.
//!
//! Hard and soft dependencies are counted differently on purpose. A hard dep
//! needs *one* provider before its dependent can build. A soft dep waits for
//! *all* batch providers, so a `Vec<Arc<dyn Handler>>` field observes the
//! complete set rather than whichever handlers happened to be built first.
//!
//! The outer `while` loop exists for re-entrancy: a component that registers
//! further components while building gets them picked up on the next pass.
//!
//! ## Resolving: which bean wins
//!
//! When several beans answer to one type, the tie-breaks apply in this order:
//!
//! | Situation | Outcome |
//! |---|---|
//! | One bean | It wins |
//! | A user bean and a `#[default_impl]` | The user bean; the default is dropped |
//! | Several beans, exactly one `#[primary]` | The primary one |
//! | Several beans, zero or ≥2 `#[primary]` | [`DiError::Ambiguous`] |
//! | Asked via `get_qualified` | The bean with the matching `#[qualifier]` |
//!
//! Two `#[primary]` beans are as ambiguous as none, so di-core reports the
//! conflict rather than silently taking the first. Use [`get_all`] when you
//! genuinely want every implementation.
//!
//! [`get_all`]: AppContext::get_all
//!
//! ## Example
//!
//! Auto-wiring, spelled out by hand. Normally `#[derive(Component)]` writes the
//! [`Buildable`] and [`RegistersComponent`] impls for you — they are written out
//! here to show what the macro actually emits.
//!
//! ```
//! use di_core::{AppContext, Buildable, Container, DiError, RegistersComponent};
//! use std::any::TypeId;
//! use std::sync::Arc;
//!
//! struct Repository;
//! struct Service { repo: Arc<Repository> }
//!
//! impl Buildable for Repository {
//!     fn build<C: Container + ?Sized>(_ctx: &C) -> Result<Arc<Self>, DiError> {
//!         Ok(Arc::new(Repository))
//!     }
//! }
//! impl RegistersComponent for Repository {}
//!
//! impl Buildable for Service {
//!     fn build<C: Container + ?Sized>(ctx: &C) -> Result<Arc<Self>, DiError> {
//!         // This is the `#[inject] repo: Arc<Repository>` field.
//!         Ok(Arc::new(Service { repo: ctx.get::<Repository>()? }))
//!     }
//! }
//! impl RegistersComponent for Service {
//!     fn dependencies() -> Vec<TypeId> { vec![TypeId::of::<Repository>()] }
//! }
//!
//! let mut ctx = AppContext::new();
//!
//! // Registered dependent-first — refresh still orders Repository ahead.
//! ctx.register_component::<Service>()
//!    .register_component::<Repository>();
//! ctx.refresh()?;
//!
//! let service = ctx.get::<Service>()?;
//! assert!(Arc::ptr_eq(&service.repo, &ctx.get::<Repository>()?));
//! # Ok::<(), DiError>(())
//! ```
//!
//! ## Performance note
//!
//! Bean lookup keys on [`TypeId`](std::any::TypeId), which is already a
//! well-distributed 128-bit value, so the container hashes it with a
//! pass-through hasher instead of SipHash. Measured at ~6× on lookups — see
//! `benches/resolve.rs`, which exists to keep that claim honest.
//!
//! ## Module map
//!
//! - [`context`] — [`AppContext`], the container itself: storage, resolution, `refresh`
//! - [`container`] — [`Container`], the read-only view a `build` receives (mockable in tests)
//! - [`buildable`] — [`Buildable`] / [`RegistersComponent`], the contract `#[derive(Component)]` implements
//! - [`bean`] — [`BeanEntry`] / [`BeanOrigin`] metadata that drives the override rules
//! - [`marker`] — marker traits the attribute macros emit
//! - [`error`] — [`DiError`]

#![forbid(unsafe_code)]

pub mod context;
pub mod container;
pub mod bean;
pub mod error;
pub mod marker;
pub mod buildable;

pub use context::AppContext;
pub use container::Container;
pub use bean::{BeanEntry, BeanOrigin};
pub use error::DiError;
pub use marker::{KernwayComponent, KernwayController};
pub use buildable::{Buildable, RegistersComponent};
