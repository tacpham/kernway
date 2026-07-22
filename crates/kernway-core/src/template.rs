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
    fn render(&self, template: &str, ctx: &dyn TemplateContext) -> Result<String, TemplateError>;
}
