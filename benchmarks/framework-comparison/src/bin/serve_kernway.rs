//! Kernway server for the framework comparison — three routes, one per workload.
//!
//!   GET  /download   → stream a fixed file (Response::file, off the blocking pool)
//!   POST /upload     → ingest the body (spooled to disk) and 200
//!   POST /multipart  → parse the form, spool the file part to disk, 200
//!
//! PORT and BENCH_FILE_MIB come from the environment so the orchestration controls them.

use std::path::PathBuf;

use di_core::RequestScope;
use kernway_core::error::StatusCode;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_server::{KernwayApp, Multipart, UploadFile};

fn main() -> std::io::Result<()> {
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let file_mib: usize = std::env::var("BENCH_FILE_MIB").ok().and_then(|s| s.parse().ok()).unwrap_or(8);

    // The file /download serves. Written once at startup.
    let download_path: PathBuf = std::env::temp_dir().join("kwbench-download.bin");
    std::fs::write(&download_path, vec![b'x'; file_mib * 1024 * 1024])?;
    let download_len = std::fs::metadata(&download_path)?.len();
    let dl_path = download_path.clone();

    println!("kernway listening on :{port}  (file {file_mib} MiB)");

    KernwayApp::builder()
        .bind(&format!("0.0.0.0:{port}"))
        // Small threshold so an upload streams to disk — the realistic large-body path.
        .max_inmemory_body(64 * 1024)
        .get("/download", move |_req: Request, _s: &RequestScope| {
            let path = dl_path.clone();
            async move { Response::new(StatusCode::OK).file(path, download_len) }
        })
        .post("/upload", |req: Request, _s: &RequestScope| async move {
            // The body was already spooled to disk by the connection task; the handle's
            // Drop removes the temp file. This mirrors "receive a file, write it, done".
            let n = UploadFile::from_request(&req).map(|u| u.len()).unwrap_or(req.body.len() as u64);
            Response::new(StatusCode::OK).body(format!("ok {n}").into_bytes())
        })
        .post("/multipart", |req: Request, _s: &RequestScope| async move {
            match Multipart::from_request(&req) {
                Ok(mut form) => {
                    let mut saved = 0u64;
                    while let Ok(Some(part)) = form.next().await {
                        if part.is_file() {
                            if let Ok(f) = part.file().await {
                                saved += f.len(); // f drops → temp file cleaned
                            }
                        }
                    }
                    Response::new(StatusCode::OK).body(format!("ok {saved}").into_bytes())
                }
                Err(e) => Response::new(StatusCode::BAD_REQUEST).body(e.to_string().into_bytes()),
            }
        })
        .build()
        .run()
}
