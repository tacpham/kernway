//! Errors, with the server's code, code name and error labels kept — the
//! retry and transaction rules depend on them.

use crate::bson::{Bson, BsonError, Document};
use kernway_orm_core::error::OrmError;
use std::fmt;

/// What went wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorKind {
    /// A bad connection string or option.
    Config,
    /// Network / socket failure (the connection is unusable).
    Network,
    /// The server spoke something unexpected.
    Protocol,
    /// Authentication failed.
    Auth,
    /// No suitable server within `serverSelectionTimeoutMS`.
    ServerSelection,
    /// The command failed (`ok: 0`).
    Command {
        /// Server error code.
        code: i32,
        /// Code name (`DuplicateKey`, `NotWritablePrimary`, …).
        code_name: String,
    },
    /// Writes failed per document (`writeErrors`) and / or the write concern
    /// was not satisfied (`writeConcernError`).
    Write {
        /// (index, code, message) per failed document.
        write_errors: Vec<(usize, i32, String)>,
        /// The write-concern failure, if any: (code, message).
        write_concern: Option<(i32, String)>,
    },
    /// BSON could not be encoded / decoded / mapped.
    Bson,
    /// A transaction / session misuse.
    Transaction,
    /// An argument the driver refuses before sending anything (an update
    /// without operators, a replacement with them, …).
    InvalidArgument,    /// Raised by application code (e.g. to abort a `with_transaction` callback).
    Application,
    /// A client bulk write failed: its write errors, write-concern errors,
    /// stopping error and partial result.
    ClientBulkWrite(Box<crate::client_bulk::ClientBulkWriteFailure>),
}

/// An error with its labels.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    /// What kind.
    pub kind: ErrorKind,
    /// Human-readable message.
    pub message: String,
    /// Server / driver error labels (`RetryableWriteError`,
    /// `TransientTransactionError`, `UnknownTransactionCommitResult`, …).
    pub labels: Vec<String>,
    /// A network error that was a timeout (SDAM never acts on those).
    pub(crate) timeout: bool,
    /// The server's reply, its `topologyVersion`, a bulk write's partial
    /// result — boxed to keep `Error` small.
    pub(crate) details: Option<Box<ErrorDetails>>,
}

/// What a server error carries beyond its kind and message.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ErrorDetails {
    /// The failed reply's `topologyVersion` (SDAM ignores stale errors).
    pub topology_version: Option<crate::topology::TopologyVersion>,
    /// The server's reply.
    pub reply: Option<Document>,
    /// What a failed bulk write did before / despite the failure.
    pub partial: Option<crate::collection::BulkWriteResult>,
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error { kind, message: message.into(), labels: Vec::new(), timeout: false, details: None }
    }
    pub(crate) fn config(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, m)
    }
    pub(crate) fn network(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, m)
    }
    pub(crate) fn protocol(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Protocol, m)
    }
    pub(crate) fn auth(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Auth, m)
    }
    pub(crate) fn transaction(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::Transaction, m)
    }
    pub(crate) fn invalid(m: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidArgument, m)
    }

    /// From a command reply with `ok: 0` (or with write errors).
    pub(crate) fn from_reply(reply: &Document) -> Self {
        let labels: Vec<String> = reply
            .get_array("errorLabels")
            .map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let code = reply.get_i64("code").unwrap_or(0) as i32;
        let mut e = Error::new(
            ErrorKind::Command { code, code_name: reply.get_str("codeName").unwrap_or("").to_string() },
            reply.get_str("errmsg").unwrap_or("command failed").to_string(),
        );
        e.labels = labels;
        e.details = Some(Box::new(ErrorDetails { topology_version: crate::topology::TopologyVersion::from_reply(reply), reply: Some(reply.clone()), partial: None }));
        e
    }

    /// An error of the application's own, e.g. returned from a
    /// `with_transaction` callback to abort the transaction.
    pub fn application(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Application, message)
    }

    /// The server error's code name (`DuplicateKey`, `UnknownReplWriteConcern`, …):
    /// the command's, else its write-concern error's, else the first write error's.
    pub fn code_name(&self) -> Option<&str> {
        if let ErrorKind::Command { code_name, .. } = &self.kind {
            return Some(code_name.as_str()).filter(|n| !n.is_empty());
        }
        let r = self.server_response()?;
        r.get_document("writeConcernError")
            .and_then(|w| w.get_str("codeName"))
            .or_else(|| r.get_array("writeErrors").and_then(|a| a.first()).and_then(Bson::as_document).and_then(|w| w.get_str("codeName")))
    }

    /// The server's reply to the failed command, when the error came from one.
    pub fn server_response(&self) -> Option<&Document> {
        self.details.as_ref()?.reply.as_ref()
    }

    /// What a failed bulk write (or `insert_many`) did: inserted, matched,
    /// modified, deleted, upserted — the parts that succeeded.
    pub fn bulk_write_result(&self) -> Option<&crate::collection::BulkWriteResult> {
        self.details.as_ref()?.partial.as_ref()
    }

    pub(crate) fn set_partial(&mut self, partial: crate::collection::BulkWriteResult) {
        self.details.get_or_insert_with(Default::default).partial = Some(partial);
    }

    /// A network timeout (`connectTimeoutMS`, `socketTimeoutMS`).
    pub(crate) fn network_timeout(m: impl Into<String>) -> Self {
        let mut e = Self::new(ErrorKind::Network, m);
        e.timeout = true;
        e
    }

    /// The error as SDAM error handling sees it (`None`: not a server
    /// failure — a bad option, a BSON error, a selection timeout…).
    /// writeErrors are never looked at, only the reply's own error or its
    /// write-concern error.
    pub(crate) fn sdam_kind(&self) -> Option<crate::topology::ApplicationErrorKind> {
        use crate::topology::ApplicationErrorKind as K;
        let command = |code: Option<i32>, message: &str| K::Command {
            code,
            message: message.to_string(),
            topology_version: self.details.as_ref().and_then(|d| d.topology_version),
            labels: self.labels.clone(),
        };
        Some(match &self.kind {
            ErrorKind::Network if self.timeout => K::Timeout,
            ErrorKind::Network | ErrorKind::Protocol => K::Network,
            ErrorKind::Command { code, .. } => command(Some(*code), &self.message),
            ErrorKind::Write { write_concern: Some((code, message)), .. } => command(Some(*code), message),
            ErrorKind::Write { .. } => command(None, ""),
            ErrorKind::Auth => command(None, &self.message),
            _ => return None,
        })
    }

    /// The server error code (command, or the first write error).
    pub fn code(&self) -> Option<i32> {
        match &self.kind {
            ErrorKind::Command { code, .. } => Some(*code),
            ErrorKind::Write { write_errors, write_concern } => write_errors.first().map(|w| w.1).or(write_concern.as_ref().map(|w| w.0)),
            _ => None,
        }
    }

    /// Whether the error carries `label`.
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }

    /// A network error (or a closed connection).
    pub fn is_network(&self) -> bool {
        self.kind == ErrorKind::Network
    }

    /// "Not primary" / node-is-recovering errors: the server's role changed.
    pub fn is_state_change(&self) -> bool {
        matches!(self.code(), Some(10107 | 13435 | 11602 | 13436 | 189 | 91 | 11600 | 10058))
            || self.message.contains("not master")
            || self.message.contains("node is recovering")
    }

    /// Errors a retryable read / write may retry once.
    pub fn is_retryable(&self) -> bool {
        self.is_network()
            || self.has_label("RetryableWriteError")
            || matches!(self.code(), Some(6 | 7 | 89 | 91 | 189 | 262 | 9001 | 10107 | 11600 | 11602 | 13435 | 13436 | 63 | 150 | 134))
    }

    /// A duplicate-key error (E11000).
    pub fn is_duplicate_key(&self) -> bool {
        self.code() == Some(11000)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ErrorKind::Command { code, code_name } => write!(f, "MongoDB error {code} ({code_name}): {}", self.message)?,
            ErrorKind::Write { write_errors, write_concern } => {
                write!(f, "MongoDB write failed: {}", self.message)?;
                for (i, c, m) in write_errors.iter().take(3) {
                    write!(f, "; document {i}: {c} {m}")?;
                }
                if let Some((c, m)) = write_concern {
                    write!(f, "; write concern: {c} {m}")?;
                }
            }
            k => write!(f, "MongoDB {k:?} error: {}", self.message)?,
        }
        if !self.labels.is_empty() {
            write!(f, " [{}]", self.labels.join(", "))?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<BsonError> for Error {
    fn from(e: BsonError) -> Self {
        Error::new(ErrorKind::Bson, e.0)
    }
}

/// The key a duplicate-key message names: `… dup key: { email: "a@x" }`.
fn dup_key_field(msg: &str) -> String {
    msg.split_once("dup key: {")
        .and_then(|(_, rest)| rest.trim_start().split(':').next())
        .map(|k| k.trim().trim_matches('"').to_string())
        .unwrap_or_default()
}

impl From<Error> for OrmError {
    fn from(e: Error) -> Self {
        if e.is_duplicate_key() {
            let msg = match &e.kind {
                ErrorKind::Write { write_errors, .. } => write_errors.first().map(|w| w.2.clone()).unwrap_or_default(),
                _ => e.message.clone(),
            };
            let field = dup_key_field(&msg);
            return OrmError::UniqueViolation { field: if field == "_id" { "id".into() } else { field } };
        }
        match e.kind {
            ErrorKind::Network | ErrorKind::ServerSelection | ErrorKind::Protocol | ErrorKind::Auth | ErrorKind::Config => {
                OrmError::Connection(e.to_string())
            }
            ErrorKind::Bson => OrmError::TypeConversion(e.to_string()),
            ErrorKind::Transaction => OrmError::Transaction(e.to_string()),
            _ if e.has_label("TransientTransactionError") => OrmError::Transaction(e.to_string()),
            _ => OrmError::Query(e.to_string()),
        }
    }
}

/// Whether a reply says `ok: 1`.
pub(crate) fn reply_ok(reply: &Document) -> bool {
    match reply.get("ok") {
        Some(Bson::Double(f)) => *f == 1.0,
        Some(b) => b.as_i64() == Some(1) || b.as_bool() == Some(true),
        None => false,
    }
}

/// Check a command reply: `ok: 1` without write errors, else the error.
pub(crate) fn check_reply(reply: Document) -> Result<Document> {
    if !reply_ok(&reply) {
        return Err(Error::from_reply(&reply));
    }
    let write_errors: Vec<(usize, i32, String)> = reply
        .get_array("writeErrors")
        .map(|a| {
            a.iter()
                .filter_map(Bson::as_document)
                .map(|w| (w.get_i64("index").unwrap_or(0) as usize, w.get_i64("code").unwrap_or(0) as i32, w.get_str("errmsg").unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    let wce = reply.get_document("writeConcernError").map(|w| (w.get_i64("code").unwrap_or(0) as i32, w.get_str("errmsg").unwrap_or("").to_string()));
    if !write_errors.is_empty() || wce.is_some() {
        let mut labels: Vec<String> = reply
            .get_array("errorLabels")
            .map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if let Some(w) = reply.get_document("writeConcernError") {
            if let Some(l) = w.get_array("errorLabels") {
                labels.extend(l.iter().filter_map(|l| l.as_str().map(String::from)));
            }
        }
        let message = write_errors.first().map(|w| w.2.clone()).or(wce.as_ref().map(|w| w.1.clone())).unwrap_or_default();
        let mut e = Error::new(ErrorKind::Write { write_errors, write_concern: wce }, message);
        e.labels = labels;
        e.details = Some(Box::new(ErrorDetails { topology_version: crate::topology::TopologyVersion::from_reply(&reply), reply: Some(reply.clone()), partial: None }));
        return Err(e);
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;

    #[test]
    fn replies_and_labels() {
        assert!(check_reply(doc! { "ok" => 1.0 }).is_ok());
        let e = check_reply(doc! { "ok" => 0.0, "code" => 10107, "codeName" => "NotWritablePrimary", "errmsg" => "not primary",
            "errorLabels" => vec![Bson::from("RetryableWriteError")] })
        .unwrap_err();
        assert!(e.is_state_change() && e.is_retryable() && e.has_label("RetryableWriteError"));
        let e = check_reply(doc! { "ok" => 1, "n" => 1, "writeErrors" => vec![Bson::Document(doc! {
            "index" => 2, "code" => 11000, "errmsg" => "E11000 duplicate key error collection: t.m index: email_1 dup key: { email: \"a@x\" }" })] })
        .unwrap_err();
        assert!(e.is_duplicate_key());
        assert!(matches!(OrmError::from(e), OrmError::UniqueViolation { field } if field == "email"));
        let e = check_reply(doc! { "ok" => 1, "writeConcernError" => doc! { "code" => 64, "errmsg" => "waiting for replication timed out" } }).unwrap_err();
        assert_eq!(e.code(), Some(64));
    }
}
