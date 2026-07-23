use crate::route_doc::RouteDoc;
use crate::spec::OpenApiSpec;

/// Collects route documentation and generates OpenAPI 3.0 JSON.
pub struct OpenApiRegistry {
    /// API title, shown as the document heading.
    pub title:       String,
    /// API version string — yours, not the OpenAPI spec version.
    pub version:     String,
    /// Long-form description of the API as a whole.
    pub description: Option<String>,
    /// Base URLs the API is served from. Defaults to `["/"]`.
    pub servers:     Vec<String>,
    pub(crate) routes: Vec<RouteDoc>,
}

impl OpenApiRegistry {
    /// Start a registry for an API with the given title and version.
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
            servers: vec!["/".to_string()],
            routes: Vec::new(),
        }
    }

    /// Set the API-level description.
    pub fn description(mut self, d: impl Into<String>) -> Self { self.description = Some(d.into()); self }
    /// Add a server URL, on top of the default `/`.
    pub fn server(mut self, url: impl Into<String>) -> Self { self.servers.push(url.into()); self }

    /// Register a route's documentation.
    pub fn add_route(&mut self, mut doc: RouteDoc, method: &str, path: &str) {
        doc.method = method.to_uppercase();
        doc.path   = path.to_string();
        self.routes.push(doc);
    }

    /// Generate OpenAPI 3.0 JSON string.
    pub fn to_json(&self) -> String {
        let spec = OpenApiSpec::build(self);
        serde_json::to_string_pretty(&spec).unwrap_or_else(|_| "{}".to_string())
    }
}
