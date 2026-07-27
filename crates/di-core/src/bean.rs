//! Bean metadata.

use std::any::TypeId;

/// Bean origin — distinguishes default beans from user beans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeanOrigin {
    /// Framework default — `#[default_impl]`. Replaced if a user bean exists.
    FrameworkDefault,
    /// User-defined — `#[component]`.
    User,
    /// Registered directly through the builder: `.register(...)`.
    Builder,
}

/// Metadata for a bean in AppContext.
#[derive(Debug, Clone)]
pub struct BeanEntry {
    /// TypeId of the concrete type.
    pub type_id: TypeId,
    /// Type name displayed in error messages.
    pub type_name: &'static str,
    /// Origin.
    pub origin: BeanOrigin,
    /// Whether this is the primary bean (when multiple implementations share a trait).
    pub is_primary: bool,
    /// Qualifier (if any).
    pub qualifier: Option<&'static str>,
}

impl BeanEntry {
    /// Create a new bean entry.
    pub fn new(type_id: TypeId, type_name: &'static str, origin: BeanOrigin) -> Self {
        Self {
            type_id,
            type_name,
            origin,
            is_primary: false,
            qualifier: None,
        }
    }

    /// Builder: set primary.
    pub fn primary(mut self) -> Self {
        self.is_primary = true;
        self
    }

    /// Builder: set qualifier.
    pub fn qualifier(mut self, q: &'static str) -> Self {
        self.qualifier = Some(q);
        self
    }

    /// Is this a framework default bean? (can be overridden)
    pub fn is_default(&self) -> bool {
        self.origin == BeanOrigin::FrameworkDefault
    }
}
