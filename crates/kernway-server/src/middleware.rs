use kernway_core::{request::Request, response::Response};

/// Sync middleware trait — intercept request/response.
/// Equivalent to HandlerInterceptor in Spring MVC.
pub trait Middleware: Send + Sync + 'static {
    /// Intercept one request/response round trip.
    ///
    /// Work before `next(req)` sees the request on the way in; work after it
    /// sees the response on the way out. Returning without calling `next`
    /// short-circuits the chain — how auth rejects a request before it reaches
    /// the handler.
    fn handle(&self, req: &mut Request, next: &dyn Fn(&mut Request) -> Response) -> Response;

    /// Short name used in logs and for conflict reporting.
    fn name(&self) -> &'static str;
}

/// Built-in: adds X-Request-Id header to request and response.
pub struct RequestIdMiddleware;

impl Middleware for RequestIdMiddleware {
    fn name(&self) -> &'static str {
        "RequestId"
    }

    fn handle(&self, req: &mut Request, next: &dyn Fn(&mut Request) -> Response) -> Response {
        let id = generate_request_id();
        req.headers.insert("x-request-id", &id);
        let mut resp = next(req);
        resp.headers.insert("x-request-id", &id);
        resp
    }
}

/// Built-in: logs method, path, status, duration.
pub struct LoggingMiddleware;

impl Middleware for LoggingMiddleware {
    fn name(&self) -> &'static str {
        "Logging"
    }

    fn handle(&self, req: &mut Request, next: &dyn Fn(&mut Request) -> Response) -> Response {
        let start = std::time::Instant::now();
        let method = req.method.clone();
        let path = req.path.clone();
        let resp = next(req);
        println!(
            "[{}] {} {} {} {}ms",
            resp.status.0,
            method,
            path,
            resp.headers.get("x-request-id").unwrap_or("-"),
            start.elapsed().as_millis()
        );
        resp
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
    use super::{LoggingMiddleware, Middleware, RequestIdMiddleware};
    use kernway_core::{error::StatusCode, request::Request, response::Response};

    #[test]
    fn request_id_middleware_adds_header() {
        let middleware = RequestIdMiddleware;
        let mut req = Request::new("GET", "/ping");

        let resp = middleware.handle(&mut req, &|request| {
            let mut resp = Response::new(StatusCode::OK);
            if let Some(id) = request.headers.get("x-request-id") {
                resp.headers.insert("seen-request-id", &id.to_string());
            }
            resp
        });

        let req_id = req.headers.get("x-request-id").unwrap().to_string();
        assert_eq!(resp.headers.get("x-request-id"), Some(req_id.as_str()));
        assert_eq!(resp.headers.get("seen-request-id"), Some(req_id.as_str()));
    }

    #[test]
    fn logging_middleware_passes_through() {
        let middleware = LoggingMiddleware;
        let mut req = Request::new("GET", "/health");

        let resp = middleware.handle(&mut req, &|_| {
            Response::new(StatusCode::NO_CONTENT)
        });

        assert_eq!(resp.status, StatusCode::NO_CONTENT);
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/health");
    }
}
