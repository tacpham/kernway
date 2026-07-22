//! KernwayApp — app builder + HTTP server.

use std::net::TcpListener;
use std::sync::Arc;

use di_core::AppContext;
use kernway_core::{error::StatusCode, request::Request, response::Response};
use kernway_http::{parse_request, write_response};

use crate::{middleware::Middleware, router::Router};

fn apply_middleware(
    middlewares: &[Arc<dyn Middleware>],
    req: &mut Request,
    endpoint: &dyn Fn(&mut Request) -> Response,
) -> Response {
    match middlewares.split_first() {
        Some((first, rest)) => first.handle(req, &|next_req| apply_middleware(rest, next_req, endpoint)),
        None => endpoint(req),
    }
}

/// App builder — fluent API similar to Spring Boot.
pub struct AppBuilder {
    addr:        String,
    router:      Router,
    context:     AppContext,
    middlewares: Vec<Arc<dyn Middleware>>,
}

impl AppBuilder {
    pub fn new() -> Self {
        Self {
            addr: "0.0.0.0:8080".to_string(),
            router: Router::new(),
            context: AppContext::new(),
            middlewares: Vec::new(),
        }
    }

    /// Listening address.
    pub fn bind(mut self, addr: &str) -> Self {
        self.addr = addr.to_string();
        self
    }

    /// Inject an AppContext with existing beans.
    pub fn context(mut self, ctx: AppContext) -> Self {
        self.context = ctx;
        self
    }

    /// Register a middleware layer.
    pub fn layer(mut self, middleware: impl Middleware) -> Self {
        self.middlewares.push(Arc::new(middleware));
        self
    }

    /// Register a GET route.
    pub fn get(mut self, pattern: &str, handler: impl Fn(&kernway_core::request::Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("GET", pattern, Arc::new(handler));
        self
    }

    /// Register a POST route.
    pub fn post(mut self, pattern: &str, handler: impl Fn(&kernway_core::request::Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("POST", pattern, Arc::new(handler));
        self
    }

    /// Register a PUT route.
    pub fn put(mut self, pattern: &str, handler: impl Fn(&kernway_core::request::Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("PUT", pattern, Arc::new(handler));
        self
    }

    /// Register a DELETE route.
    pub fn delete(mut self, pattern: &str, handler: impl Fn(&kernway_core::request::Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("DELETE", pattern, Arc::new(handler));
        self
    }

    /// Register a PATCH route.
    pub fn patch(mut self, pattern: &str, handler: impl Fn(&kernway_core::request::Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("PATCH", pattern, Arc::new(handler));
        self
    }

    /// Build and return KernwayApp.
    pub fn build(self) -> KernwayApp {
        KernwayApp {
            addr: self.addr,
            router: Arc::new(self.router),
            context: Arc::new(self.context),
            middlewares: Arc::new(self.middlewares),
        }
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// HTTP application.
pub struct KernwayApp {
    addr:        String,
    router:      Arc<Router>,
    context:     Arc<AppContext>,
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
}

impl KernwayApp {
    pub fn builder() -> AppBuilder {
        AppBuilder::new()
    }

    /// Start the server — blocks until the process is killed.
    pub fn run(self) {
        let listener = TcpListener::bind(&self.addr)
            .unwrap_or_else(|e| panic!("cannot bind {}: {}", self.addr, e));

        println!("🚀  Kernway listening on http://{}", self.addr);
        println!("     Press Ctrl+C to stop\n");

        for stream in listener.incoming() {
            match stream {
                Ok(mut tcp_stream) => {
                    let router = Arc::clone(&self.router);
                    let context = Arc::clone(&self.context);
                    let middlewares = Arc::clone(&self.middlewares);

                    std::thread::spawn(move || {
                        let mut request = match parse_request(&tcp_stream) {
                            Ok(request) => request,
                            Err(err) => {
                                let resp = Response::new(StatusCode::BAD_REQUEST)
                                    .content_type("text/plain")
                                    .body(err.to_string().into_bytes());
                                write_response(&mut tcp_stream, &resp);
                                return;
                            }
                        };

                        let endpoint = |req: &mut Request| match router.find(&req.method, &req.path) {
                            Some((handler, params)) => {
                                req.path_params = params;
                                handler(req, &context)
                            }
                            None => Response::new(StatusCode::NOT_FOUND)
                                .content_type("application/json")
                                .body(
                                    format!(r#"{{"error":"no route for {} {}"}}"#, req.method, req.path)
                                        .into_bytes(),
                                ),
                        };

                        let response = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            apply_middleware(middlewares.as_ref().as_slice(), &mut request, &endpoint)
                        })) {
                            Ok(response) => response,
                            Err(_) => Response::new(StatusCode::INTERNAL_SERVER_ERROR)
                                .content_type("application/json")
                                .body(br#"{"error":"internal server error"}"#.to_vec()),
                        };

                        write_response(&mut tcp_stream, &response);
                    });
                }
                Err(err) => eprintln!("connection error: {}", err),
            }
        }
    }
}
