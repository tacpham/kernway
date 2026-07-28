# Layered Architecture

> How to structure a Kernway web app in clear layers — and how to split a larger
> system into cooperating services — the way the Spring apps you know are organised.

Kernway is unopinionated about layout, but a **layered** structure keeps a growing
app manageable. This is the convention we recommend; the `headless-cms` reference
app is built exactly this way.

## The layers of one app

Inside a single binary, separate concerns into modules that depend **downward**
only (controller → service → repository), with cross-cutting `config` and `view`:

```
my-app/
├── application.yml            # config, read by kernway-config
└── src/
    ├── main.rs                # bootstrap: wire the object graph, register controllers, run
    ├── config.rs              # #[configuration] typed views over application.yml
    ├── controller/            # #[controller]/#[route] — HTTP in, Response out. No business logic.
    │   ├── mod.rs
    │   └── home.rs
    ├── service/               # business logic. No HTTP, no SQL literals — just the domain.
    │   ├── mod.rs
    │   └── catalog.rs
    ├── repository/            # data access (an ORM Repository, or an HTTP client to another service)
    ├── model/                 # entities + DTOs (often a shared crate)
    └── view.rs                # template engine (kernleaf) + a render helper
```

**Rule of thumb:** a controller parses the request and renders a response; it
delegates every decision to a service. A service owns the logic and calls a
repository. Nothing calls *upward*.

## Wiring: constructor injection

Each layer receives its dependencies through its constructor — the same idea as
Spring's `@Autowired`, but explicit. Build the graph once in `main` and register
the controllers:

```rust
// main.rs
let view    = Arc::new(View::new());
let repo    = Arc::new(CatalogRepository::new(driver));
let catalog = Arc::new(CatalogService::new(Arc::clone(&repo)));
let home    = Arc::new(HomeController::new(Arc::clone(&catalog), Arc::clone(&view)));

KernwayApp::builder()
    .bind("0.0.0.0:8080")
    .controller(home)
    .build()
    .run()
```

A controller is a struct whose fields are its dependencies:

```rust
// controller/home.rs
use di_macro::controller;
use kernway_server::{Request, Response};

pub struct HomeController {
    catalog: Arc<CatalogService>,
    view:    Arc<View>,
}

#[controller("")]
impl HomeController {
    #[route(GET, "/")]
    async fn home(&self, _req: Request) -> Response {
        let books = self.catalog.list().await;
        self.view.render("index", &model(&books))
    }
}
```

> Prefer this explicit wiring for clarity. When the graph gets large, Kernway's DI
> container (`#[component]`/`#[inject]`, see [Dependency Injection](../reference/di-system.md))
> can auto-wire it for you — the layering is identical either way.

## Typed configuration

Put configuration in `application.yml` and bind sections to structs with
`#[configuration]` (Spring's `@ConfigurationProperties`) rather than reading keys
by hand in `main`:

```rust
use kernway_config::configuration;

#[configuration(prefix = "server")]
#[derive(Default)]
pub struct ServerConfig { pub port: Option<u16> }

let port = ServerConfig::from_config(&cfg).port.unwrap_or(8080);
```

## Security as a layer, not scattered checks

Don't check auth inside every handler. Declare access rules once with
`HttpSecurity`/`SecurityLayer`, let an auth middleware populate the
`SecurityContext`, and gate methods with `#[require_role(...)]`. See
[Authentication & Authorization](authentication.md).

## Splitting into cooperating services

When one binary is not the right unit — different scaling, different hosts, a
sensitive backend you want off the public edge — split along the **data boundary**:
one service *owns* the store; the others are thin web tiers that call it over HTTP.

`headless-cms` is organised this way as a Cargo workspace:

```
headless-cms/
├── common/          # shared model/DTOs (one crate, no duplication)
├── async-service/   # owns Meilisearch + auth; a JSON HTTP API. Runs where the data is.
├── async-worker/    # writes/crawls into the store
├── user-dashboard/  # public UI  — calls async-service over HTTP, no DB access
└── admin-dashboard/ # admin UI   — calls async-service over HTTP
```

Only `async-service` (and the worker) touch the database and hold the secrets; the
dashboards are pure web tiers. Each crate is itself layered
(config → controller → service → view). The shared model lives in `common`, so the
service serialises and the dashboards deserialise the *same* type. Keep heavy media
out of the JSON path — pass URLs (or a streaming `/media` proxy), not bytes.

Benefits: the store and its credentials never reach the public tier; the tiers
deploy and scale independently; and the boundary is a small, testable HTTP contract.

## See also

- [Project Structure](../getting-started/project-structure.md) — the single-app layout
- [For Spring Developers](../getting-started/for-spring-developers.md)
- [Spring Migration](spring-migration.md)
- [Dependency Injection](../reference/di-system.md)
- [Authentication & Authorization](authentication.md)
