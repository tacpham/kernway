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
    /// Whether this handle owns the temp file's cleanup. A body upload does not
    /// (`false`): the request's [`SpooledBody`](kernway_core::request::SpooledBody)
    /// deletes it on drop. A `multipart` file part does (`true`): nothing else
    /// tracks it, so an un-persisted part is removed when this handle drops.
    owns_file: bool,
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
            // The request's SpooledBody owns cleanup; this is only a handle.
            Some(spool) => Ok(Self { path: spool.path.clone(), len: spool.len, owns_file: false }),
            None => Err("no streamed upload (body empty or under max_inmemory_body)".into()),
        }
    }

    /// Build an `UploadFile` from a temp file that some other producer spooled —
    /// a `multipart/form-data` file part, not the whole request body. Same
    /// ownership contract as a body upload: the file is moved by [`persist`], and
    /// otherwise removed when the request ends.
    ///
    /// [`persist`]: UploadFile::persist
    pub(crate) fn from_spooled(path: PathBuf, len: u64) -> Self {
        Self { path, len, owns_file: true }
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
            None => Err(std::io::Error::other("blocking pool unavailable")),
        }
    }
}

impl Drop for UploadFile {
    fn drop(&mut self) {
        // A multipart file part owns its temp file (nothing else tracks it), so an
        // un-persisted one is cleaned up here. `persist` already moved the file, so
        // this is a no-op after a successful persist. A body upload sets `owns_file`
        // false — its SpooledBody in the request handles cleanup.
        if self.owns_file {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
