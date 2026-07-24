//! Authorization — the facts a `th:authorize` and a route guard check.

/// What the current request is allowed to do.
///
/// Defined here, in the spec crate, so a template engine (`kernleaf`) can consult
/// it through this trait without depending on the security crate that produces
/// it. `kernway-security`'s `SecurityContext` implements it; an application with
/// its own principal type can too. Keeping the trait tiny — is-authenticated and
/// has-role — is deliberate: richer checks (`hasAnyRole`, `permitAll`) are built
/// on top of these two, by the caller.
pub trait Authorization {
    /// Whether the request is authenticated (has a known principal).
    fn is_authenticated(&self) -> bool;

    /// Whether the principal holds `role`.
    fn has_role(&self, role: &str) -> bool;
}

/// The fail-closed default: an unauthenticated request with no roles. What a
/// template evaluates `th:authorize` against when no context was supplied, so a
/// missing security context denies rather than grants.
#[derive(Debug, Clone, Copy, Default)]
pub struct Anonymous;

impl Authorization for Anonymous {
    fn is_authenticated(&self) -> bool {
        false
    }
    fn has_role(&self, _role: &str) -> bool {
        false
    }
}
