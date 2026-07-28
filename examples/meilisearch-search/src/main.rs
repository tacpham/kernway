//! meilisearch-search — a search-as-you-type demo over the Meilisearch ORM,
//! showcasing a **custom, user-supplied string primary key**.
//!
//! The entity is a book whose id is its ISBN (e.g. `9780134685991`) — a unique
//! string the caller sets, not an auto-generated number. `Entity::Id` is `String`
//! and `#[id]` sits on the `isbn` field, so that value becomes the Meilisearch
//! primary key.
//!
//! Type in the box; htmx fires `GET /search?q=…` on each (debounced) keystroke,
//! the handler runs a full-text query via
//! `Repository::query().filter_like().fetch_all()`, and the results dropdown
//! swaps in — no page reload, no JavaScript of our own.
//!
//! ## Run it
//!
//! ```bash
//! docker run --rm -p 7700:7700 -e MEILI_MASTER_KEY=masterKey getmeili/meilisearch:v1.7
//! MEILI_URL=http://localhost:7700 MEILI_API_KEY=masterKey cargo run -p meilisearch-search
//! open http://localhost:8080
//! ```
//!
//! On startup the index is seeded (best-effort — if Meilisearch is unreachable
//! the server still starts and search just returns an error row).

use std::sync::Arc;

use kernway_core::error::StatusCode;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_orm_core::{driver::Driver, repository::Repository};
use kernway_orm_macro::entity;
use kernway_orm_meilisearch::driver::MeilisearchDriver;
use kernway_server::{KernwayApp, RequestScope};
use serde::{Deserialize, Serialize};

/// A book, keyed by its **ISBN** — a unique string the caller chooses. `#[id]` on
/// `isbn` (a `String`) makes it the Meilisearch primary key; there is no
/// auto-generated numeric id. Title and author are full-text searchable by
/// default.
#[entity(table = "books")]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Book {
    /// The user-supplied primary key — an ISBN-13, e.g. `9780134685991`.
    #[id()]
    isbn: String,
    /// Book title (searchable).
    title: String,
    /// Author (searchable).
    author: String,
    /// Price in whole currency units.
    price: f64,
}

/// The seed catalogue — each row's first column is the custom string id (ISBN).
fn catalogue() -> Vec<Book> {
    let rows = [
        ("9780134685991", "Effective Java", "Joshua Bloch", 45.0),
        ("9781718500440", "The Rust Programming Language", "Klabnik and Nichols", 40.0),
        ("9781492052593", "Programming Rust", "Blandy, Orendorff and Tindall", 55.0),
        ("9780132350884", "Clean Code", "Robert C. Martin", 42.0),
        ("9780135957059", "The Pragmatic Programmer", "Hunt and Thomas", 50.0),
        ("9781449373320", "Designing Data-Intensive Applications", "Martin Kleppmann", 55.0),
        ("9780262046305", "Introduction to Algorithms", "Cormen et al.", 90.0),
        ("9780134190440", "The Go Programming Language", "Donovan and Kernighan", 38.0),
        ("9780134757599", "Refactoring", "Martin Fowler", 48.0),
        ("9780321125217", "Domain-Driven Design", "Eric Evans", 52.0),
        ("9780735619678", "Code Complete", "Steve McConnell", 44.0),
        ("9780262510875", "Structure and Interpretation of Computer Programs", "Abelson and Sussman", 35.0),
    ];
    rows.into_iter()
        .map(|(isbn, title, author, price)| Book {
            isbn: isbn.to_string(),
            title: title.to_string(),
            author: author.to_string(),
            price,
        })
        .collect()
}

fn main() -> std::io::Result<()> {
    let url = std::env::var("MEILI_URL").unwrap_or_else(|_| "http://localhost:7700".to_string());
    let key = std::env::var("MEILI_API_KEY").unwrap_or_else(|_| "masterKey".to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());

    let driver = Arc::new(MeilisearchDriver::connect(url.clone(), key));

    seed(Arc::clone(&driver), &url);

    println!("listening on http://localhost:{port}  (Meilisearch at {url})");

    let search_driver = Arc::clone(&driver);
    KernwayApp::builder()
        .bind(&format!("0.0.0.0:{port}"))
        .get("/", |_req: Request, _scope: &RequestScope| async move { page() })
        .get("/search", move |req: Request, _scope: &RequestScope| {
            let driver = Arc::clone(&search_driver);
            async move { search(&req, driver.as_ref()).await }
        })
        .build()
        .run()
}

/// Seed the index once at startup on a throwaway executor (the app builds its own
/// runtime afterwards). Best-effort: a failure here is logged, not fatal.
fn seed(driver: Arc<MeilisearchDriver>, url: &str) {
    let ex = match rt_core::Executor::new() {
        Ok(ex) => ex,
        Err(e) => {
            eprintln!("seed: could not start an executor: {e}");
            return;
        }
    };
    let result = ex.block_on(async move {
        let repo: Box<dyn Repository<Book>> = driver.repository();
        repo.save_all(catalogue()).await
    });
    match result {
        Ok(Ok(saved)) => println!("seeded {} books", saved.len()),
        Ok(Err(e)) => eprintln!("seed skipped — is Meilisearch running at {url}? ({e})"),
        Err(_) => eprintln!("seed executor panicked"),
    }
}

/// `GET /search?q=…` — full-text search, returns the results as an HTML fragment.
async fn search(req: &Request, driver: &MeilisearchDriver) -> Response {
    let q = req.query.get("q").unwrap_or("").trim();
    if q.is_empty() {
        return html("".to_string());
    }
    let repo: Box<dyn Repository<Book>> = driver.repository();
    match repo.query().filter_like("title", q).limit(8).fetch_all().await {
        Ok(items) if items.is_empty() => {
            html(format!("<li class=\"empty\">No matches for “{}”.</li>", escape(q)))
        }
        Ok(items) => html(items.iter().map(row).collect::<String>()),
        Err(e) => html(format!("<li class=\"error\">Search error: {}</li>", escape(&e.to_string()))),
    }
}

/// One result row. The custom string id (the ISBN) is shown as a tag to make the
/// user-supplied primary key visible.
fn row(b: &Book) -> String {
    format!(
        "<li><span class=\"title\">{}</span>\
         <span class=\"author\">{}</span>\
         <code class=\"id\">{}</code>\
         <span class=\"price\">${:.0}</span></li>",
        escape(&b.title),
        escape(&b.author),
        escape(&b.isbn),
        b.price
    )
}

/// The single page: a search box wired to `/search` via htmx, and an empty
/// results list it swaps into. htmx debounces input by 250 ms.
fn page() -> Response {
    html(PAGE.to_string())
}

const PAGE: &str = r##"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Book search</title>
<script src="https://unpkg.com/htmx.org@2.0.3"></script>
<style>
  body { font: 16px/1.5 system-ui, sans-serif; max-width: 42rem; margin: 4rem auto; padding: 0 1rem; }
  h1 { font-size: 1.3rem; }
  input[type=search] { width: 100%; padding: .7rem .9rem; font-size: 1rem;
    border: 1px solid #ccc; border-radius: .5rem; box-sizing: border-box; }
  #results { list-style: none; margin: .4rem 0 0; padding: 0;
    border: 1px solid #eee; border-radius: .5rem; overflow: hidden; }
  #results:empty { display: none; }
  #results li { display: flex; gap: .75rem; align-items: baseline;
    padding: .55rem .9rem; border-top: 1px solid #f0f0f0; }
  #results li:first-child { border-top: 0; }
  #results .title { flex: 1; font-weight: 600; }
  #results .author { color: #888; font-size: .85rem; }
  #results .id { color: #a15; background: #faf0f4; font-size: .72rem;
    padding: .05rem .35rem; border-radius: .25rem; }
  #results .price { color: #333; font-variant-numeric: tabular-nums; }
  #results .empty, #results .error { color: #888; justify-content: center; }
  #results .error { color: #b00; }
  .htmx-request#results { opacity: .6; }
</style>
</head><body>
<h1>Book search <small style="color:#999;font-weight:400">— keyed by ISBN</small></h1>
<input type="search" name="q" placeholder="Search books… (try “rust”, “clean”, “algorithms”)"
       autocomplete="off" autofocus
       hx-get="/search"
       hx-trigger="input changed delay:250ms, search"
       hx-target="#results"
       hx-swap="innerHTML"
       hx-indicator="#results">
<ul id="results"></ul>
</body></html>"##;

/// Minimal HTML-escaping for text placed in element content.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html(body: String) -> Response {
    Response::new(StatusCode::OK)
        .content_type("text/html; charset=utf-8")
        .body(body.into_bytes())
}
