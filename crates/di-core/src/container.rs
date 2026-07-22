//! Container — the read-side abstraction over a DI context.

use std::any::Any;
use std::sync::Arc;

use crate::error::DiError;

/// Read-only view of a bean container, used during component construction.
///
/// [`AppContext`](crate::AppContext) implements this. Abstracting
/// [`Buildable::build`](crate::Buildable::build) over `Container` lets a component
/// be constructed against **any** container — the concrete `AppContext`, a
/// child/scoped context that falls back to a parent, or a mock in unit tests —
/// instead of being hard-wired to one struct.
///
/// # Object safety
/// Not object-safe (generic methods), so use it as a bound (`C: Container`) or
/// `&impl Container`, not `&dyn Container`.
///
/// # Example — building a component against a mock
/// ```rust,ignore
/// struct Mock { repo: Arc<Repo> }
/// impl Container for Mock { /* return the canned Repo from get::<Repo>() */ }
/// let svc = Service::build(&Mock { repo }).unwrap();   // no AppContext needed
/// ```
pub trait Container {
    /// Resolve a single bean by concrete type.
    fn get<T: Any + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError>;

    /// Resolve a bean by qualifier (name).
    fn get_qualified<T: Any + Send + Sync + 'static>(
        &self,
        qualifier: &str,
    ) -> Result<Arc<T>, DiError>;

    /// Resolve a bean by the trait it was registered under (`Arc<dyn Trait>`).
    fn get_as<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError>;

    /// All beans of a concrete type (empty if none).
    fn get_all<T: Any + Send + Sync + 'static>(&self) -> Vec<Arc<T>>;

    /// All beans bound to a trait (empty if none).
    fn get_all_as<T: ?Sized + Send + Sync + 'static>(&self) -> Vec<Arc<T>>;
}
