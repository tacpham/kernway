//! # kernway-openapi
//!
//! Describes your routes, then emits an OpenAPI 3.0 document — the input a
//! Swagger UI or a client generator wants.
//!
//! ## The idea
//!
//! Spring reads annotations off loaded classes at runtime to work out what an
//! endpoint accepts. Without reflection there is nothing to read, so the
//! description is built explicitly: you attach a [`RouteDoc`] to a route, the
//! registry collects them, and the spec is folded out of that collection.
//!
//! More typing than an annotation, and one honest advantage: the description
//! cannot silently disagree with a type the reflector guessed wrong about.
//!
//! ## The flow
//!
//! ```text
//!   RouteDoc::new("List users")            per route, built fluently
//!       .tag("users")
//!       .query_param("page", …)
//!       .response_json(200, …, "#/components/schemas/User")
//!               │
//!               │  registry.add_route(doc, "GET", "/users")
//!               ▼                          ← method and path are filled in here
//!   OpenApiRegistry   { title, version, servers, routes: Vec<RouteDoc> }
//!               │
//!               │  to_json()
//!               ▼
//!   OpenApiSpec ──serde──►  openapi.json  ──►  Swagger UI / codegen
//! ```
//!
//! Routes sharing a path merge into one path item keyed by method, which is
//! what the spec requires — so `GET /users` and `POST /users` end up as two
//! operations under a single `/users` entry.
//!
//! ## Example
//!
//! ```
//! use kernway_openapi::{OpenApiRegistry, RouteDoc};
//!
//! let mut api = OpenApiRegistry::new("Users API", "1.0.0")
//!     .description("Everything about users");
//!
//! api.add_route(
//!     RouteDoc::new("Fetch one user")
//!         .tag("users")
//!         .path_param("id", "User id", "integer")
//!         .response_json(200, "The user", "#/components/schemas/User")
//!         .response(404, "No such user"),
//!     "GET",
//!     "/users/{id}",
//! );
//!
//! let json = api.to_json();
//! assert!(json.contains("\"/users/{id}\""));
//! assert!(json.contains("\"get\""));
//! ```
//!
//! ## Schemas are references, not definitions
//!
//! [`ResponseDoc::schema_ref`] and [`RequestBodyDoc::schema_ref`] hold strings
//! like `#/components/schemas/User`. This crate does not derive a schema from a
//! Rust type — it records the pointer and trusts the target exists. Deriving
//! schemas from `#[derive(Serialize)]` types is future work; until then a typo
//! in a `schema_ref` surfaces in the UI, not at compile time.
//!
//! [`ResponseDoc::schema_ref`]: route_doc::ResponseDoc::schema_ref
//! [`RequestBodyDoc::schema_ref`]: route_doc::RequestBodyDoc::schema_ref
//!
//! ## Module map
//!
//! - [`route_doc`] — [`RouteDoc`] and its parts: what you write per route
//! - [`registry`] — [`OpenApiRegistry`]: collects routes, renders JSON
//! - [`spec`] — [`OpenApiSpec`]: the serializable document itself

/// Collects route docs and renders the final JSON.
pub mod registry;
/// Per-route documentation types.
pub mod route_doc;
/// The serializable OpenAPI 3.0 document.
pub mod spec;

pub use registry::OpenApiRegistry;
pub use route_doc::{ParamDoc, ParamIn, RequestBodyDoc, ResponseDoc, RouteDoc};
pub use spec::OpenApiSpec;
