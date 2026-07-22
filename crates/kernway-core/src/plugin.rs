//! Plugin system — extend Kernway without forking.

/// Plugin interface — adds functionality to the app builder.
///
/// Equivalent to `ApplicationContextInitializer` in Spring.
pub trait KernwayPlugin: Send + Sync {
    /// Plugin name — used for logging and conflict detection.
    fn name(&self) -> &'static str;

    /// Plugin version.
    fn version(&self) -> &'static str {
        "0.0.0"
    }
}
