use crate::registry::OpenApiRegistry;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize)]
/// The serializable OpenAPI 3.0 document.
///
/// Built from an [`OpenApiRegistry`] rather than
/// assembled by hand — this type exists to be serialized, not edited.
pub struct OpenApiSpec {
    openapi: &'static str,
    info:    Info,
    servers: Vec<Server>,
    paths:   HashMap<String, PathItem>,
}

#[derive(Serialize)]
struct Info {
    title:       String,
    version:     String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Serialize)]
struct Server { url: String }

#[derive(Serialize, Default)]
struct PathItem {
    #[serde(skip_serializing_if = "Option::is_none")] get:    Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")] post:   Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")] put:    Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")] delete: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")] patch:  Option<Operation>,
}

#[derive(Serialize)]
struct Operation {
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags:         Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parameters:   Vec<Parameter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "requestBody")]
    request_body: Option<RequestBody>,
    responses:    HashMap<String, Response>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    deprecated:   bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "operationId")]
    operation_id: Option<String>,
}

#[derive(Serialize)]
struct Parameter {
    name:        String,
    #[serde(rename = "in")]
    location:    String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    required:    bool,
    schema:      Schema,
}

#[derive(Serialize)]
struct Schema {
    #[serde(rename = "type")]
    schema_type: String,
}

#[derive(Serialize)]
struct RequestBody {
    description: String,
    required:    bool,
    content:     HashMap<String, MediaType>,
}

#[derive(Serialize)]
struct MediaType {
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<SchemaRef>,
}

#[derive(Serialize)]
enum SchemaRef {
    Ref { #[serde(rename = "$ref")] reference: String },
}

#[derive(Serialize)]
struct Response {
    description: String,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    content:     HashMap<String, MediaType>,
}

impl OpenApiSpec {
    /// Fold every registered route into one OpenAPI document.
    ///
    /// Routes sharing a path are merged into a single path item keyed by
    /// method, as the spec requires.
    pub fn build(registry: &OpenApiRegistry) -> Self {
        let mut paths: HashMap<String, PathItem> = HashMap::new();

        for route in &registry.routes {
            let path_item = paths.entry(route.path.clone()).or_default();

            let mut responses = HashMap::new();
            if route.responses.is_empty() {
                responses.insert("200".to_string(), Response {
                    description: "OK".to_string(), content: HashMap::new(),
                });
            }
            for r in &route.responses {
                let mut content = HashMap::new();
                if let Some(ct) = &r.content_type {
                    content.insert(ct.clone(), MediaType {
                        schema: r.schema_ref.as_ref().map(|s| SchemaRef::Ref { reference: s.clone() }),
                    });
                }
                responses.insert(r.status.to_string(), Response {
                    description: r.description.clone(), content,
                });
            }

            let params = route.params.iter().map(|p| Parameter {
                name: p.name.clone(),
                location: format!("{:?}", p.location).to_lowercase(),
                description: p.description.clone(),
                required: p.required,
                schema: Schema { schema_type: p.schema_type.clone() },
            }).collect();

            let request_body = route.request_body.as_ref().map(|b| {
                let mut content = HashMap::new();
                content.insert(b.content_type.clone(), MediaType {
                    schema: b.schema_ref.as_ref().map(|s| SchemaRef::Ref { reference: s.clone() }),
                });
                RequestBody { description: b.description.clone(), required: b.required, content }
            });

            let op = Operation {
                summary: route.summary.clone(),
                description: route.description.clone(),
                tags: route.tags.clone(),
                parameters: params,
                request_body,
                responses,
                deprecated: route.deprecated,
                operation_id: route.operation_id.clone(),
            };

            match route.method.as_str() {
                "GET" => path_item.get = Some(op),
                "POST" => path_item.post = Some(op),
                "PUT" => path_item.put = Some(op),
                "DELETE" => path_item.delete = Some(op),
                "PATCH" => path_item.patch = Some(op),
                _ => {}
            }
        }

        Self {
            openapi: "3.0.3",
            info: Info {
                title:       registry.title.clone(),
                version:     registry.version.clone(),
                description: registry.description.clone(),
            },
            servers: registry.servers.iter().map(|u| Server { url: u.clone() }).collect(),
            paths,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{route_doc::RouteDoc, registry::OpenApiRegistry};

    fn make_registry() -> OpenApiRegistry {
        let mut r = OpenApiRegistry::new("Test API", "1.0.0");
        r.add_route(
            RouteDoc::new("Get user")
                .tag("users")
                .path_param("id", "User ID", "integer")
                .response_json(200, "User found", "#/components/schemas/User")
                .response(404, "User not found"),
            "GET", "/users/{id}",
        );
        r.add_route(
            RouteDoc::new("Create user")
                .tag("users")
                .body_json("User data", "#/components/schemas/CreateUser")
                .response_json(201, "User created", "#/components/schemas/User"),
            "POST", "/users",
        );
        r
    }

    #[test]
    fn openapi_version_is_3_0_3() {
        let r = make_registry();
        let json = r.to_json();
        assert!(json.contains(r#""openapi": "3.0.3""#));
    }

    #[test]
    fn info_title_and_version_present() {
        let r = make_registry();
        let json = r.to_json();
        assert!(json.contains("Test API"));
        assert!(json.contains("1.0.0"));
    }

    #[test]
    fn get_route_in_paths() {
        let r = make_registry();
        let json = r.to_json();
        assert!(json.contains("/users/{id}"));
        assert!(json.contains("Get user"));
    }

    #[test]
    fn post_route_in_paths() {
        let r = make_registry();
        let json = r.to_json();
        assert!(json.contains("/users"));
        assert!(json.contains("Create user"));
    }

    #[test]
    fn path_param_in_spec() {
        let r = make_registry();
        let json = r.to_json();
        assert!(json.contains(r#""in": "path""#));
        assert!(json.contains(r#""name": "id""#));
    }

    #[test]
    fn response_codes_present() {
        let r = make_registry();
        let json = r.to_json();
        assert!(json.contains(r#""200""#));
        assert!(json.contains(r#""404""#));
        assert!(json.contains(r#""201""#));
    }

    #[test]
    fn tags_present() {
        let r = make_registry();
        let json = r.to_json();
        assert!(json.contains("users"));
    }

    #[test]
    fn empty_registry_produces_valid_json() {
        let r = OpenApiRegistry::new("Empty", "0.1.0");
        let json = r.to_json();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["openapi"], "3.0.3");
    }
}
