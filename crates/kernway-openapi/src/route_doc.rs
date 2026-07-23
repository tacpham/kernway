use serde::{Deserialize, Serialize};

/// Where a parameter lives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamIn {
    /// Captured from the route pattern (`/users/{id}`). Always required.
    Path,
    /// Read from the query string.
    Query,
    /// Read from a request header.
    Header,
    /// Read from a cookie.
    Cookie,
}

/// A single parameter doc.
#[derive(Debug, Clone, Serialize)]
pub struct ParamDoc {
    /// Parameter name as the client sends it.
    pub name:        String,
    /// Where the parameter is read from. Serialized as `in`, per the spec.
    #[serde(rename = "in")]
    pub location:    ParamIn,
    /// Human-readable explanation shown in the generated docs.
    pub description: Option<String>,
    /// Whether the request is invalid without it. Path params are always true.
    pub required:    bool,
    /// JSON Schema type name — `"string"`, `"integer"`, `"boolean"`, ...
    pub schema_type: String,
}

/// Response documentation for one status code.
#[derive(Debug, Clone, Serialize)]
pub struct ResponseDoc {
    /// The HTTP status this entry documents.
    pub status:       u16,
    /// What this status means for this route.
    pub description:  String,
    /// Media type of the body, if there is one. `None` means no body.
    pub content_type: Option<String>,
    /// Reference to a schema component, e.g. `#/components/schemas/User`.
    pub schema_ref:   Option<String>,
}

/// Request body documentation.
#[derive(Debug, Clone, Serialize)]
pub struct RequestBodyDoc {
    /// What the body carries.
    pub description:  String,
    /// Media type the endpoint accepts.
    pub content_type: String,
    /// Whether the request is invalid without a body.
    pub required:     bool,
    /// Reference to a schema component describing the body.
    pub schema_ref:   Option<String>,
}

/// Full documentation for one route.
#[derive(Debug, Clone)]
pub struct RouteDoc {
    /// HTTP method, uppercased. Filled in by
    /// [`OpenApiRegistry::add_route`](crate::OpenApiRegistry::add_route).
    pub method:       String,
    /// Route pattern. Filled in by the registry alongside `method`.
    pub path:         String,
    /// One-line summary — the headline in the generated UI.
    pub summary:      String,
    /// Longer prose shown when the operation is expanded.
    pub description:  Option<String>,
    /// Tags used to group operations in the UI.
    pub tags:         Vec<String>,
    /// Path, query, header, and cookie parameters.
    pub params:       Vec<ParamDoc>,
    /// The request body, for methods that take one.
    pub request_body: Option<RequestBodyDoc>,
    /// One entry per documented status code.
    pub responses:    Vec<ResponseDoc>,
    /// Marks the operation as deprecated — struck through in the UI.
    pub deprecated:   bool,
    /// Stable identifier for client generators. Must be unique across the spec.
    pub operation_id: Option<String>,
}

impl RouteDoc {
    /// Start building a route doc. Method and path are set by the registry.
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            method: String::new(),
            path: String::new(),
            summary: summary.into(),
            description: None,
            tags: Vec::new(),
            params: Vec::new(),
            request_body: None,
            responses: Vec::new(),
            deprecated: false,
            operation_id: None,
        }
    }

    /// Set the long-form description.
    pub fn description(mut self, d: impl Into<String>) -> Self { self.description = Some(d.into()); self }
    /// Add a grouping tag. Call more than once to belong to several groups.
    pub fn tag(mut self, t: impl Into<String>) -> Self { self.tags.push(t.into()); self }
    /// Mark the operation deprecated.
    pub fn deprecated(mut self) -> Self { self.deprecated = true; self }
    /// Set the operation id used by client generators.
    pub fn operation_id(mut self, id: impl Into<String>) -> Self { self.operation_id = Some(id.into()); self }

    /// Document a path parameter. Always recorded as required, since a route
    /// cannot match without it.
    pub fn path_param(mut self, name: impl Into<String>, desc: impl Into<String>, schema_type: impl Into<String>) -> Self {
        self.params.push(ParamDoc {
            name: name.into(), location: ParamIn::Path,
            description: Some(desc.into()), required: true, schema_type: schema_type.into(),
        });
        self
    }

    /// Document a query parameter.
    pub fn query_param(mut self, name: impl Into<String>, desc: impl Into<String>, schema_type: impl Into<String>, required: bool) -> Self {
        self.params.push(ParamDoc {
            name: name.into(), location: ParamIn::Query,
            description: Some(desc.into()), required, schema_type: schema_type.into(),
        });
        self
    }

    /// Document a response that carries no body — a 204, or an error whose
    /// shape you do not want to pin down.
    pub fn response(mut self, status: u16, desc: impl Into<String>) -> Self {
        self.responses.push(ResponseDoc {
            status, description: desc.into(), content_type: None, schema_ref: None,
        });
        self
    }

    /// Document a JSON response, pointing at a schema component.
    pub fn response_json(mut self, status: u16, desc: impl Into<String>, schema_ref: impl Into<String>) -> Self {
        self.responses.push(ResponseDoc {
            status, description: desc.into(),
            content_type: Some("application/json".to_string()),
            schema_ref: Some(schema_ref.into()),
        });
        self
    }

    /// Document a required JSON request body.
    pub fn body_json(mut self, desc: impl Into<String>, schema_ref: impl Into<String>) -> Self {
        self.request_body = Some(RequestBodyDoc {
            description: desc.into(),
            content_type: "application/json".to_string(),
            required: true,
            schema_ref: Some(schema_ref.into()),
        });
        self
    }
}
