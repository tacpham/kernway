//! DI errors.

use thiserror::Error;

/// Error that occurs during DI resolution.
#[derive(Debug, Error)]
pub enum DiError {
    /// No bean found for the requested type.
    #[error("no bean found for `{type_name}` — did you forget #[component]?")]
    NotFound { type_name: &'static str },

    /// Multiple beans exist for the same type — requires #[primary] or #[qualifier].
    #[error("multiple beans found for `{type_name}` — add #[primary] to one of them")]
    Ambiguous { type_name: &'static str },

    /// Circular dependency.
    #[error("circular dependency detected: {cycle}")]
    CircularDependency { cycle: String },

    /// A `register_component` bean depends on a type nobody provides.
    #[error("missing dependency for `{type_name}` — no registered bean or #[provides] supplies it")]
    MissingDependency { type_name: &'static str },

    /// The registered instance's concrete type does not match the entry's `TypeId`.
    ///
    /// Guards `get`/`get_qualified` from ever downcasting to the wrong type.
    #[error("type mismatch registering `{type_name}`: instance type does not match the entry TypeId")]
    TypeMismatch { type_name: &'static str },
}
