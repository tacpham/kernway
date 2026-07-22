use serde::{Deserialize, Serialize};

/// Where a parameter lives.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamIn { Path, Query, Header, Cookie }

/// A single parameter doc.
#[derive(Debug, Clone, Serialize)]
pub struct ParamDoc {
    pub name:        String,
    #[serde(rename = "in")]
    pub location:    ParamIn,
    pub description: Option<String>,
    pub required:    bool,
    pub schema_type: String,
}

/// Response documentation for one status code.
#[derive(Debug, Clone, Serialize)]
pub struct ResponseDoc {
    pub status:       u16,
    pub description:  String,
    pub content_type: Option<String>,
    pub schema_ref:   Option<String>,
}

/// Request body documentation.
#[derive(Debug, Clone, Serialize)]
pub struct RequestBodyDoc {
    pub description:  String,
    pub content_type: String,
    pub required:     bool,
    pub schema_ref:   Option<String>,
}

/// Full documentation for one route.
#[derive(Debug, Clone)]
pub struct RouteDoc {
    pub method:       String,
    pub path:         String,
    pub summary:      String,
    pub description:  Option<String>,
    pub tags:         Vec<String>,
    pub params:       Vec<ParamDoc>,
    pub request_body: Option<RequestBodyDoc>,
    pub responses:    Vec<ResponseDoc>,
    pub deprecated:   bool,
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

    pub fn description(mut self, d: impl Into<String>) -> Self { self.description = Some(d.into()); self }
    pub fn tag(mut self, t: impl Into<String>) -> Self { self.tags.push(t.into()); self }
    pub fn deprecated(mut self) -> Self { self.deprecated = true; self }
    pub fn operation_id(mut self, id: impl Into<String>) -> Self { self.operation_id = Some(id.into()); self }

    pub fn path_param(mut self, name: impl Into<String>, desc: impl Into<String>, schema_type: impl Into<String>) -> Self {
        self.params.push(ParamDoc {
            name: name.into(), location: ParamIn::Path,
            description: Some(desc.into()), required: true, schema_type: schema_type.into(),
        });
        self
    }

    pub fn query_param(mut self, name: impl Into<String>, desc: impl Into<String>, schema_type: impl Into<String>, required: bool) -> Self {
        self.params.push(ParamDoc {
            name: name.into(), location: ParamIn::Query,
            description: Some(desc.into()), required, schema_type: schema_type.into(),
        });
        self
    }

    pub fn response(mut self, status: u16, desc: impl Into<String>) -> Self {
        self.responses.push(ResponseDoc {
            status, description: desc.into(), content_type: None, schema_ref: None,
        });
        self
    }

    pub fn response_json(mut self, status: u16, desc: impl Into<String>, schema_ref: impl Into<String>) -> Self {
        self.responses.push(ResponseDoc {
            status, description: desc.into(),
            content_type: Some("application/json".to_string()),
            schema_ref: Some(schema_ref.into()),
        });
        self
    }

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
