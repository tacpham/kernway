use crate::route_doc::RouteDoc;
use crate::spec::OpenApiSpec;

/// Collects route documentation and generates OpenAPI 3.0 JSON.
pub struct OpenApiRegistry {
    pub title:       String,
    pub version:     String,
    pub description: Option<String>,
    pub servers:     Vec<String>,
    pub(crate) routes: Vec<RouteDoc>,
}

impl OpenApiRegistry {
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
            servers: vec!["/".to_string()],
            routes: Vec::new(),
        }
    }

    pub fn description(mut self, d: impl Into<String>) -> Self { self.description = Some(d.into()); self }
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
