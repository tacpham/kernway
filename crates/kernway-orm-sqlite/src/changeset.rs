//! Change capture with SQLite's session extension: record what a
//! transaction changed as a compact binary changeset, ship it elsewhere,
//! apply it to another database, invert it to undo, or merge several.
//!
//! A session watches one connection, so capture is tied to a transaction
//! (which owns one connection) — see
//! [`SqliteDriver::track_changes`](crate::SqliteDriver::track_changes). Only
//! tables with a declared `PRIMARY KEY` are recorded (SQLite's rule: a change
//! is identified by its key).

use crate::config::ChangeKind;
use crate::worker::map_err;
use kernway_orm_core::error::OrmError;
use crate::ffi;
use std::os::raw::c_int;

/// What a capture records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeFormat {
    /// Old and new values of every changed column: can be inverted, and
    /// detects every conflict when applied.
    Changeset,
    /// Primary key and new values only: smaller, cannot be inverted.
    Patchset,
}

/// A recorded set of changes, as SQLite's portable binary format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changeset {
    bytes: Vec<u8>,
    format: ChangeFormat,
}

/// One change inside a [`Changeset`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeOp {
    /// The table.
    pub table: String,
    /// Insert, update or delete.
    pub kind: ChangeKind,
    /// Made by a trigger or a foreign-key action rather than a statement.
    pub indirect: bool,
}

/// What to do when a change does not fit the target database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    /// Stop and undo the whole apply (the default).
    Abort,
    /// Leave the target row as it is and go on.
    Skip,
    /// Overwrite the target row where SQLite allows it (a row that changed
    /// in between, or a key already present); otherwise skip.
    Replace,
}

/// The result of applying a changeset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Applied {
    /// Changes that met a conflict (skipped or replaced per [`OnConflict`]).
    pub conflicts: u64,
}

impl Changeset {
    /// Wrap bytes received from elsewhere (as produced by [`as_bytes`](Self::as_bytes)).
    pub fn from_bytes(bytes: Vec<u8>, format: ChangeFormat) -> Self {
        Self { bytes, format }
    }

    /// The binary changeset.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The binary changeset, owned.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Changeset or patchset.
    pub fn format(&self) -> ChangeFormat {
        self.format
    }

    /// Nothing changed.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The changeset that undoes this one (inserts become deletes, updates
    /// swap old and new values). Patchsets cannot be inverted.
    pub async fn invert(&self) -> Result<Changeset, OrmError> {
        if self.format == ChangeFormat::Patchset {
            return Err(OrmError::Unsupported("a patchset has no old values to invert".into()));
        }
        let bytes = self.bytes.clone();
        offload(move || {
            let out = crate::raw::invert_changeset(&bytes).map_err(map_err)?;
            Ok(Changeset { bytes: out, format: ChangeFormat::Changeset })
        })
        .await
    }

    /// One changeset with the effect of `self` followed by `next` (changes to
    /// the same row are merged).
    pub async fn concat(&self, next: &Changeset) -> Result<Changeset, OrmError> {
        if self.format != next.format {
            return Err(OrmError::Query("cannot concatenate a changeset and a patchset".into()));
        }
        let (a, b, format) = (self.bytes.clone(), next.bytes.clone(), self.format);
        offload(move || {
            let out = crate::raw::concat_changesets(&a, &b).map_err(map_err)?;
            Ok(Changeset { bytes: out, format })
        })
        .await
    }

    /// The changes it holds, in order.
    pub async fn operations(&self) -> Result<Vec<ChangeOp>, OrmError> {
        let bytes = self.bytes.clone();
        offload(move || {
            if bytes.is_empty() {
                return Ok(Vec::new());
            }
            let ops = crate::raw::changeset_ops(&bytes).map_err(map_err)?;
            Ok(ops
                .into_iter()
                .filter_map(|(table, op, indirect)| {
                    let kind = match op {
                        ffi::SQLITE_INSERT => ChangeKind::Insert,
                        ffi::SQLITE_UPDATE => ChangeKind::Update,
                        ffi::SQLITE_DELETE => ChangeKind::Delete,
                        _ => return None,
                    };
                    Some(ChangeOp { table, kind, indirect })
                })
                .collect())
        })
        .await
    }
}

/// Changeset work is pure CPU over bytes: off the reactor.
async fn offload<T: Send + 'static>(f: impl FnOnce() -> Result<T, OrmError> + Send + 'static) -> Result<T, OrmError> {
    rt_core::spawn_blocking(f).await.unwrap_or_else(|| Err(OrmError::Query("panic while processing a changeset".into())))
}

/// The action SQLite should take for one conflict of kind `kind` under
/// `policy`.
pub(crate) fn resolve(policy: OnConflict, kind: c_int) -> c_int {
    match policy {
        OnConflict::Abort => ffi::SQLITE_CHANGESET_ABORT,
        OnConflict::Skip => ffi::SQLITE_CHANGESET_OMIT,
        // REPLACE is only valid for these two kinds.
        OnConflict::Replace => match kind {
            ffi::SQLITE_CHANGESET_DATA | ffi::SQLITE_CHANGESET_CONFLICT => ffi::SQLITE_CHANGESET_REPLACE,
            _ => ffi::SQLITE_CHANGESET_OMIT,
        },
    }
}
