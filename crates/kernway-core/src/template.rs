//! Template engine abstraction.

use std::collections::HashMap;

/// Template render error.
#[derive(Debug)]
pub struct TemplateError(pub String);

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "template error: {}", self.0)
    }
}

impl std::error::Error for TemplateError {}

/// Context data for template rendering — key/value pairs.
pub trait TemplateContext {
    /// Look up one value by name, as referenced from the template.
    ///
    /// Returns `None` for an unknown key — an engine decides for itself whether
    /// that renders as empty or is an error.
    fn get(&self, key: &str) -> Option<&dyn std::any::Any>;
}

/// Blanket implementation for HashMap.
impl TemplateContext for HashMap<String, Box<dyn std::any::Any>> {
    fn get(&self, key: &str) -> Option<&dyn std::any::Any> {
        self.get(key).map(|v| v.as_ref())
    }
}

/// Template engine — renders a template file into an HTML string.
///
/// Equivalent to `ViewResolver` + `TemplateEngine` in Spring MVC/Thymeleaf.
/// `KernleafEngine` implements this trait.
pub trait TemplateEngine: Send + Sync {
    /// Render `template` (a name the engine resolves to a file) against `ctx`.
    ///
    /// Implementations are expected to HTML-escape interpolated values by
    /// default, so a template cannot become an XSS vector by accident.
    fn render(&self, template: &str, ctx: &dyn TemplateContext) -> Result<String, TemplateError>;
}
