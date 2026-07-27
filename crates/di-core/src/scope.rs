//! Request scope — a per-request [`Container`] layered over the [`AppContext`].
//!
//! Per [KEP-0005]. Beans set here live for one request (a `SecurityContext`, a CSRF
//! token, a request id); resolution checks the request-local map first and falls
//! back to the application singletons in the parent. Because a Kernway task is
//! pinned to its core for its whole life, the scope is created and dropped on one
//! thread — no `ThreadLocal`, no scoped proxy.
//!
//! It is `Send + Sync` (an `RwLock` + `Arc` beans) so it can be held across the
//! `await` points of the async middleware chain, which requires `Send`. On a single
//! core the lock is uncontended.
//!
//! [KEP-0005]: https://github.com/tacpham/kernway/blob/main/docs/kep/0005-request-scoped-beans.md

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::container::Container;
use crate::context::AppContext;
use crate::error::DiError;

/// A request-scoped bean instance.
type Instance = Arc<dyn Any + Send + Sync>;

/// A per-request DI scope. Set request-scoped beans with [`set`](Self::set); resolve
/// with the [`Container`] methods, which fall back to the parent [`AppContext`] for
/// application singletons.
pub struct RequestScope<'a> {
    local: RwLock<HashMap<TypeId, Instance>>,
    parent: &'a AppContext,
}

impl<'a> RequestScope<'a> {
    /// A fresh, empty scope over the application context.
    pub fn new(parent: &'a AppContext) -> Self {
        Self {
            local: RwLock::new(HashMap::new()),
            parent,
        }
    }

    /// Put a request-scoped bean by value.
    pub fn set<T: Any + Send + Sync + 'static>(&self, value: T) {
        self.local
            .write()
            .unwrap()
            .insert(TypeId::of::<T>(), Arc::new(value));
    }

    /// Put a request-scoped bean already wrapped in an `Arc`.
    pub fn insert<T: Any + Send + Sync + 'static>(&self, value: Arc<T>) {
        self.local.write().unwrap().insert(TypeId::of::<T>(), value);
    }

    /// Whether a request-scoped bean of this type has been set.
    pub fn has<T: Any + Send + Sync + 'static>(&self) -> bool {
        self.local.read().unwrap().contains_key(&TypeId::of::<T>())
    }

    /// The application context this scope layers over.
    pub fn parent(&self) -> &AppContext {
        self.parent
    }

    fn local_get<T: Any + Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.local
            .read()
            .unwrap()
            .get(&TypeId::of::<T>())
            .map(|inst| {
                Arc::clone(inst)
                    .downcast::<T>()
                    .expect("RequestScope invariant: stored type matches its TypeId key")
            })
    }

    // Inherent resolution — request-local beans first, then the application
    // singletons. Mirrors `AppContext`'s inherent methods, so a handler calls
    // `scope.get::<T>()` without importing the `Container` trait.

    /// Resolve a bean by concrete type: this request's, else the application's.
    pub fn get<T: Any + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        match self.local_get::<T>() {
            Some(local) => Ok(local),
            None => self.parent.get::<T>(),
        }
    }

    /// Resolve a bean by qualifier — from the application context (request beans
    /// are unqualified).
    pub fn get_qualified<T: Any + Send + Sync + 'static>(
        &self,
        qualifier: &str,
    ) -> Result<Arc<T>, DiError> {
        self.parent.get_qualified::<T>(qualifier)
    }

    /// Resolve a trait-object bean — from the application context.
    pub fn get_as<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        self.parent.get_as::<T>()
    }

    /// All beans of a concrete type — the application's, plus this request's if set.
    pub fn get_all<T: Any + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        let mut all = self.parent.get_all::<T>();
        if let Some(local) = self.local_get::<T>() {
            all.push(local);
        }
        all
    }

    /// All beans bound to a trait — from the application context.
    pub fn get_all_as<T: ?Sized + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        self.parent.get_all_as::<T>()
    }
}

/// Delegates to the inherent methods (via explicit `RequestScope::…` paths, never
/// the trait, so there is no self-recursion) — same pattern as `AppContext`.
impl Container for RequestScope<'_> {
    fn get<T: Any + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        RequestScope::get::<T>(self)
    }
    fn get_qualified<T: Any + Send + Sync + 'static>(
        &self,
        qualifier: &str,
    ) -> Result<Arc<T>, DiError> {
        RequestScope::get_qualified::<T>(self, qualifier)
    }
    fn get_as<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>, DiError> {
        RequestScope::get_as::<T>(self)
    }
    fn get_all<T: Any + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        RequestScope::get_all::<T>(self)
    }
    fn get_all_as<T: ?Sized + Send + Sync + 'static>(&self) -> Vec<Arc<T>> {
        RequestScope::get_all_as::<T>(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppContext {
        let mut ctx = AppContext::new();
        ctx.register_instance::<String>(Arc::new("singleton".to_string()))
            .unwrap();
        ctx
    }

    #[test]
    fn a_request_bean_resolves_from_the_scope() {
        let app = app();
        let scope = RequestScope::new(&app);
        scope.set(42u64);
        assert_eq!(*scope.get::<u64>().unwrap(), 42);
    }

    #[test]
    fn a_singleton_falls_through_to_the_parent() {
        let app = app();
        let scope = RequestScope::new(&app);
        assert_eq!(*scope.get::<String>().unwrap(), "singleton");
    }

    #[test]
    fn a_missing_bean_is_not_found() {
        let app = app();
        let scope = RequestScope::new(&app);
        assert!(matches!(scope.get::<u32>(), Err(DiError::NotFound { .. })));
    }

    #[test]
    fn each_scope_is_independent() {
        let app = app();
        let a = RequestScope::new(&app);
        let b = RequestScope::new(&app);
        a.set(1u64);
        b.set(2u64);
        assert_eq!(*a.get::<u64>().unwrap(), 1);
        assert_eq!(*b.get::<u64>().unwrap(), 2);
        assert!(
            !RequestScope::new(&app).has::<u64>(),
            "a fresh scope has no request beans"
        );
    }

    #[test]
    fn set_can_shadow_a_singleton_for_one_request() {
        let app = app();
        let scope = RequestScope::new(&app);
        // No app-wide u64; set one just for this request.
        assert!(scope.get::<u64>().is_err());
        scope.set(7u64);
        assert_eq!(*scope.get::<u64>().unwrap(), 7);
    }

    #[test]
    fn the_scope_is_send_and_sync() {
        // Compile-time assertion: it must be Send + Sync to cross async middleware awaits.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RequestScope<'_>>();
    }
}
