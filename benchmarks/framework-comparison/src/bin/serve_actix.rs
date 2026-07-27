//! actix-web server for the framework comparison — same three routes.
//!
//!   GET  /download   → actix-files NamedFile (streamed)
//!   POST /upload     → stream the body to a temp file, then remove it, 200
//!   POST /multipart  → actix-multipart, write the file field to a temp file, 200

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use actix_files::NamedFile;
use actix_multipart::Multipart;
use actix_web::{web, App, HttpServer, Responder};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("kwbench-actix-{tag}-{}-{n}.tmp", std::process::id()))
}

async fn download(path: web::Data<PathBuf>) -> actix_web::Result<NamedFile> {
    Ok(NamedFile::open_async(path.get_ref()).await?)
}

async fn upload(mut payload: web::Payload) -> impl Responder {
    let path = temp_path("up");
    let mut file = tokio::fs::File::create(&path).await.expect("create temp");
    let mut total = 0u64;
    while let Some(chunk) = payload.next().await {
        let chunk = chunk.expect("body chunk");
        total += chunk.len() as u64;
        file.write_all(&chunk).await.expect("write");
    }
    file.flush().await.ok();
    tokio::fs::remove_file(&path).await.ok();
    format!("ok {total}")
}

async fn multipart(mut payload: Multipart) -> impl Responder {
    let mut saved = 0u64;
    while let Some(field) = payload.next().await {
        let mut field = field.expect("field");
        let is_file = field.content_disposition().and_then(|cd| cd.get_filename()).is_some();
        if is_file {
            let path = temp_path("mp");
            let mut file = tokio::fs::File::create(&path).await.expect("create temp");
            while let Some(chunk) = field.next().await {
                let chunk = chunk.expect("chunk");
                saved += chunk.len() as u64;
                file.write_all(&chunk).await.expect("write");
            }
            file.flush().await.ok();
            tokio::fs::remove_file(&path).await.ok();
        }
    }
    format!("ok {saved}")
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8082".into());
    let file_mib: usize = std::env::var("BENCH_FILE_MIB").ok().and_then(|s| s.parse().ok()).unwrap_or(8);

    let download_path = std::env::temp_dir().join("kwbench-download.bin");
    std::fs::write(&download_path, vec![b'x'; file_mib * 1024 * 1024])?;
    let dl = web::Data::new(download_path);

    println!("actix listening on :{port}  (file {file_mib} MiB)");
    HttpServer::new(move || {
        App::new()
            .app_data(dl.clone())
            .route("/download", web::get().to(download))
            .route("/upload", web::post().to(upload))
            .route("/multipart", web::post().to(multipart))
    })
    .bind(format!("0.0.0.0:{port}"))?
    .run()
    .await
}
