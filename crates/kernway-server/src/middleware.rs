use std::sync::Arc;

use di_core::RequestScope;
use kernway_core::layer::BoxFuture;
use kernway_core::{request::Request, response::Response};

/// The terminal of the chain — runs the matched handler. Returns a `'static`
/// future (the handler owns the request and whatever it pulled from the scope).
pub(crate) type Terminal<'a> =
    dyn Fn(Request, &RequestScope) -> BoxFuture<'static, Response> + Sync + 'a;

/// The continuation handed to a middleware: the rest of the chain, then the
/// handler. Moved (not borrowed) into `Middleware::handle`, so a middleware's
/// future can own it without a self-referential borrow.
pub struct Next<'a> {
    pub(crate) rest: &'a [Arc<dyn Middleware>],
    pub(crate) terminal: &'a Terminal<'a>,
}

impl<'a> Next<'a> {
    /// Run the next middleware, or the handler when the chain is exhausted.
    pub fn run(self, req: Request, scope: &'a RequestScope) -> BoxFuture<'a, Response> {
        match self.rest.split_first() {
            Some((first, rest)) => first.handle(req, scope, Next { rest, terminal: self.terminal }),
            None => (self.terminal)(req, scope),
        }
    }
}

/// Async middleware ([KEP-0006]) — intercept a request/response round trip.
///
/// Equivalent to `HandlerInterceptor` in Spring MVC. Do work before
/// `next.run(req, scope).await`, work after it on the response, or return without
/// calling `next` to reject early. `scope` is the per-request DI scope
/// ([KEP-0005]): set a request-scoped bean (a `SecurityContext`) and the handler
/// injects it. Because the handler may `await`, so does the chain.
///
/// [KEP-0005]: https://github.com/tacpham/kernway/blob/main/docs/kep/0005-request-scoped-beans.md
/// [KEP-0006]: https://github.com/tacpham/kernway/blob/main/docs/kep/0006-async-handlers.md
pub trait Middleware: Send + Sync + 'static {
    /// Intercept one request/response round trip.
    fn handle<'a>(
        &'a self,
        req: Request,
        scope: &'a RequestScope,
        next: Next<'a>,
    ) -> BoxFuture<'a, Response>;

    /// Short name used in logs and for conflict reporting.
    fn name(&self) -> &'static str;
}

/// Built-in: adds X-Request-Id header to request and response.
pub struct RequestIdMiddleware;

impl Middleware for RequestIdMiddleware {
    fn name(&self) -> &'static str {
        "RequestId"
    }

    fn handle<'a>(&'a self, mut req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        Box::pin(async move {
            let id = generate_request_id();
            req.headers.insert("x-request-id", &id);
            let mut resp = next.run(req, scope).await;
            resp.headers.insert("x-request-id", &id);
            resp
        })
    }
}

/// Built-in: logs method, path, status, duration.
pub struct LoggingMiddleware;

impl Middleware for LoggingMiddleware {
    fn name(&self) -> &'static str {
        "Logging"
    }

    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        Box::pin(async move {
            let start = std::time::Instant::now();
            let method = req.method.clone();
            let path = req.path.clone();
            let resp = next.run(req, scope).await;
            // The access log line, through the framework logger (KW_LOG controls it).
            kernway_log::info!(
                target: "kernway_server",
                "{method} {path} -> {} ({}ms) req={}",
                resp.status.0,
                start.elapsed().as_millis(),
                resp.headers.get("x-request-id").unwrap_or("-"),
            );
            resp
        })
    }
}

fn generate_request_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:x}-{:x}", t.as_secs(), t.subsec_nanos())
}

#[cfg(test)]
mod tests {
    use super::{LoggingMiddleware, Middleware, Next, RequestIdMiddleware, Terminal};
    use di_core::{AppContext, RequestScope};
    use kernway_core::layer::BoxFuture;
    use kernway_core::{error::StatusCode, request::Request, response::Response};

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(f).unwrap()
    }

    #[test]
    fn request_id_middleware_adds_header() {
        let middleware = RequestIdMiddleware;
        let app = AppContext::new();
        let scope = RequestScope::new(&app);

        // The terminal echoes back the x-request-id the middleware put on the request.
        let terminal: &Terminal = &|request: Request, _scope: &RequestScope| {
            let seen = request.headers.get("x-request-id").map(str::to_string);
            Box::pin(async move {
                let mut resp = Response::new(StatusCode::OK);
                if let Some(id) = seen {
                    resp.headers.insert("seen-request-id", &id);
                }
                resp
            }) as BoxFuture<'static, Response>
        };
        let next = Next { rest: &[], terminal };
        let resp = block_on(middleware.handle(Request::new("GET", "/ping"), &scope, next));

        // The middleware set x-request-id on the response, and the terminal saw the
        // same value on the request.
        let id = resp.headers.get("x-request-id").unwrap().to_string();
        assert_eq!(resp.headers.get("seen-request-id"), Some(id.as_str()));
    }

    #[test]
    fn logging_middleware_passes_through() {
        let middleware = LoggingMiddleware;
        let app = AppContext::new();
        let scope = RequestScope::new(&app);

        let terminal: &Terminal = &|_req: Request, _scope: &RequestScope| {
            Box::pin(async { Response::new(StatusCode::NO_CONTENT) }) as BoxFuture<'static, Response>
        };
        let next = Next { rest: &[], terminal };
        let resp = block_on(middleware.handle(Request::new("GET", "/health"), &scope, next));

        assert_eq!(resp.status, StatusCode::NO_CONTENT);
    }
}
