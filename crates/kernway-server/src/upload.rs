//! `UploadFile` — a large request body the server streamed to a temporary file.
//!
//! Bodies over `max_inmemory_body` (configured on the `AppBuilder`) are streamed to disk
//! by the connection task rather than buffered in memory, so a multi-GB upload stays
//! O(chunk). A handler receives the result as an [`UploadFile`] argument.

use std::path::{Path, PathBuf};

use kernway_core::request::Request;

/// A large uploaded body the server streamed to a temporary file. The handler gets the
/// file path instead of the bytes, so the upload never sits in memory.
///
/// ```rust,ignore
/// async fn upload(&self, file: UploadFile) -> impl IntoResponse {
///     file.persist("/data/song.mp3").await?;   // move it into place, off the shard
///     StatusCode::CREATED
/// }
/// ```
///
/// The temp file is removed automatically when the request ends unless [`persist`] moved
/// it out first.
///
/// [`persist`]: UploadFile::persist
#[derive(Debug)]
pub struct UploadFile {
    path: PathBuf,
    len: u64,
}

impl UploadFile {
    /// Extract the spooled upload from the request.
    ///
    /// # Errors
    /// Returns an error when the request carried no streamed body — it was empty, or small
    /// enough (≤ `max_inmemory_body`) to be read into memory, in which case `req.body` /
    /// `Json` / `Validated` apply instead.
    pub fn from_request(req: &Request) -> Result<Self, String> {
        match &req.body_spool {
            Some(spool) => Ok(Self { path: spool.path.clone(), len: spool.len }),
            None => Err("no streamed upload (body empty or under max_inmemory_body)".into()),
        }
    }

    /// Path to the temp file holding the uploaded bytes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The upload size in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the upload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Move the upload to `dest`, consuming it — a rename when `dest` shares the temp
    /// dir's filesystem (instant), otherwise a copy then delete. Runs on the blocking
    /// pool so the shard never stalls on the move. Point `upload_temp_dir` at your
    /// storage volume to keep this a rename.
    ///
    /// # Errors
    /// Returns any I/O error from the rename or copy.
    pub async fn persist(self, dest: impl Into<PathBuf>) -> std::io::Result<()> {
        let dest = dest.into();
        let src = self.path.clone();
        let moved = rt_core::spawn_blocking(move || match std::fs::rename(&src, &dest) {
            Ok(()) => Ok(()),
            // Cross-device: fall back to copy + remove.
            Err(_) => {
                std::fs::copy(&src, &dest)?;
                std::fs::remove_file(&src)
            }
        })
        .await;
        match moved {
            Some(result) => result,
            None => Err(std::io::Error::new(std::io::ErrorKind::Other, "blocking pool unavailable")),
        }
    }
}
