# kernway-core — Trait Specifications

> This crate contains only trait definitions. Compile time is < 1s. It includes no implementation code.

## Standards

- RFC 9110 (HTTP Semantics) — `IntoResponse`, status codes
- RFC 9112 (HTTP/1.1) — `Request`, `Response` types
- JSR-330 patterns — DI trait design (inspiration, not binding)

## Core Traits

```rust
/// Convert a value into an HTTP response.
/// Implements the HttpMessageConverter concept (analogous to Spring's HttpMessageConverter).
pub trait IntoResponse: Send {
    fn into_response(self) -> Response;
}

/// Extract a typed value from an HTTP request.
/// Analogous to Spring's HandlerMethodArgumentResolver.
pub trait FromRequest: Sized {
    type Error: IntoResponse;
    fn from_request(req: &Request) -> Result<Self, Self::Error>;
}

/// Render a named template with a context value.
/// Analogous to Spring's ViewResolver.
pub trait TemplateEngine: Send + Sync {
    fn render(&self, template: &str, ctx: &dyn TemplateContext) -> Result<String, TemplateError>;
    fn supports(&self, template: &str) -> bool;
}

/// Marker trait for template context values.
pub trait TemplateContext: Send + Sync {
    fn get_field(&self, name: &str) -> Option<&dyn std::any::Any>;
}

/// Async database connection pool.
/// Analogous to javax.sql.DataSource.
pub trait DbPool: Send + Sync {
    fn acquire(&self) -> BoxFuture<'_, Result<Box<dyn Connection>, DbError>>;
    fn release(&self, conn: Box<dyn Connection>);
}

/// Middleware layer — wraps a request handler.
/// Analogous to javax.servlet.Filter.
pub trait Layer: Send + Sync {
    fn handle<'a>(
        &'a self,
        req: Request,
        next: &'a dyn Next,
    ) -> BoxFuture<'a, Response>;
}

/// Plugin that registers capabilities into the app builder.
/// Analogous to Spring's AutoConfiguration.
pub trait KernwayPlugin: Send + Sync {
    fn register(&self, app: &mut AppBuilder);
    fn name(&self) -> &'static str;
    fn priority(&self) -> i32 { 0 }
}
```

## Types

```rust
/// Minimal HTTP request representation.
/// Fields populated by http-proto crate before handler dispatch.
pub struct Request {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Body,
    pub extensions: Extensions,  // type-erased DI beans, extractors
}

/// Minimal HTTP response.
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Body,
}

pub enum Body {
    Empty,
    Bytes(Bytes),
    Stream(Box<dyn AsyncRead + Send + Unpin>),
}
```

## Rules

1. Do not import serde, serde_json, diesel, rustls, or any implementation crate
2. No `#[cfg]` blocks
3. No default implementations unless they are clearly documented
4. All trait items must have doc comments with RFC references (if applicable)
