//! axum + tower-http server for the framework comparison — same three routes.
//!
//!   GET  /download   → tower_http ServeFile (streamed)
//!   POST /upload     → stream the body to a temp file, then remove it, 200
//!   POST /multipart  → axum Multipart, write the file field to a temp file, 200

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{DefaultBodyLimit, Multipart};
use axum::body::Body;
use axum::response::IntoResponse;
use axum::routing::{get_service, post};
use axum::Router;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tower_http::services::ServeFile;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("kwbench-axum-{tag}-{}-{n}.tmp", std::process::id()))
}

/// Stream a request body to disk, then remove the file — the write is the work.
async fn upload(body: Body) -> impl IntoResponse {
    let path = temp_path("up");
    let mut file = tokio::fs::File::create(&path).await.expect("create temp");
    let mut total = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("body chunk");
        total += chunk.len() as u64;
        file.write_all(&chunk).await.expect("write");
    }
    file.flush().await.ok();
    tokio::fs::remove_file(&path).await.ok();
    format!("ok {total}")
}

/// Parse a multipart form; write each file field to disk, then remove it.
async fn multipart(mut mp: Multipart) -> impl IntoResponse {
    let mut saved = 0u64;
    while let Ok(Some(mut field)) = mp.next_field().await {
        if field.file_name().is_some() {
            let path = temp_path("mp");
            let mut file = tokio::fs::File::create(&path).await.expect("create temp");
            while let Ok(Some(chunk)) = field.chunk().await {
                saved += chunk.len() as u64;
                file.write_all(&chunk).await.expect("write");
            }
            file.flush().await.ok();
            tokio::fs::remove_file(&path).await.ok();
        }
    }
    format!("ok {saved}")
}

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8081".into());
    let file_mib: usize = std::env::var("BENCH_FILE_MIB").ok().and_then(|s| s.parse().ok()).unwrap_or(8);

    let download_path = std::env::temp_dir().join("kwbench-download.bin");
    std::fs::write(&download_path, vec![b'x'; file_mib * 1024 * 1024]).expect("write file");

    let app = Router::new()
        .route("/download", get_service(ServeFile::new(&download_path)))
        .route("/upload", post(upload))
        .route("/multipart", post(multipart))
        // axum caps request bodies at 2 MB by default; the other two servers accept
        // large uploads, so disable it here to compare the same work.
        .layer(DefaultBodyLimit::disable());

    let addr = format!("0.0.0.0:{port}");
    println!("axum listening on :{port}  (file {file_mib} MiB)");
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    axum::serve(listener, app).await.expect("serve");
}
