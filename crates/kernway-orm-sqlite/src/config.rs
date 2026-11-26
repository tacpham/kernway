//! Where the database is and how each connection is set up.

use kernway_orm_core::error::OrmError;
use kernway_orm_sql::Value;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Where the database lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A database file (created on first open unless the mode forbids it).
    File(PathBuf),
    /// A private in-memory database. Every connection of one driver sees the
    /// same data (SQLite's `memdb` VFS); it disappears when the driver is
    /// dropped. The name only has to be unique within the process.
    Memory(String),
}

/// How the database is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Read and write; create the file if missing (the default).
    ReadWriteCreate,
    /// Read and write an existing file.
    ReadWrite,
    /// Read only. Writes fail; transactions are plain (`BEGIN DEFERRED`).
    ReadOnly,
}

/// `PRAGMA journal_mode`. `Wal` (the default for files) lets readers run
/// while one writer writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalMode {
    /// Write-ahead log.
    Wal,
    /// Rollback journal, deleted at commit (SQLite's own default).
    Delete,
    /// Rollback journal, truncated at commit.
    Truncate,
    /// Rollback journal, header zeroed at commit.
    Persist,
    /// Rollback journal in memory.
    Memory,
    /// No journal: no rollback, no crash safety.
    Off,
}

impl JournalMode {
    fn sql(self) -> &'static str {
        match self {
            Self::Wal => "WAL",
            Self::Delete => "DELETE",
            Self::Truncate => "TRUNCATE",
            Self::Persist => "PERSIST",
            Self::Memory => "MEMORY",
            Self::Off => "OFF",
        }
    }
}

/// `PRAGMA synchronous`: how hard SQLite waits for the disk at commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Synchronous {
    /// Never wait (a power loss can corrupt the database).
    Off,
    /// The default here: with WAL, durable except the last commits on power loss.
    Normal,
    /// Wait at every commit.
    Full,
    /// `Full`, plus the directory after a journal is deleted.
    Extra,
}

impl Synchronous {
    fn sql(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Normal => "NORMAL",
            Self::Full => "FULL",
            Self::Extra => "EXTRA",
        }
    }
}

/// The body of a SQL function written in Rust: the arguments in, a value or
/// an error message out.
pub type FunctionBody = dyn Fn(&[Value]) -> Result<Value, String> + Send + Sync;

/// A Rust function callable from SQL on every connection — SQLite's answer
/// to stored functions (it has no procedural language).
#[derive(Clone)]
pub struct ScalarFunction {
    pub(crate) name: String,
    pub(crate) args: i32,
    pub(crate) deterministic: bool,
    pub(crate) body: Arc<FunctionBody>,
}

impl fmt::Debug for ScalarFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScalarFunction").field("name", &self.name).field("args", &self.args).finish_non_exhaustive()
    }
}

/// One row changed, as reported to [`SqliteConfig::on_change`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// What happened to the row.
    pub kind: ChangeKind,
    /// The database (`main`, or an attached one's name).
    pub database: String,
    /// The table.
    pub table: String,
    /// The row's rowid.
    pub rowid: i64,
}

/// The kind of a [`Change`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A row was inserted.
    Insert,
    /// A row was updated.
    Update,
    /// A row was deleted.
    Delete,
}

type ChangeHook = Arc<dyn Fn(&Change) + Send + Sync>;
type EndHook = Arc<dyn Fn() + Send + Sync>;

/// Callbacks run on the connection thread as SQLite reports changes.
#[derive(Clone, Default)]
pub(crate) struct Hooks {
    pub(crate) change: Option<ChangeHook>,
    pub(crate) commit: Option<EndHook>,
    pub(crate) rollback: Option<EndHook>,
}

impl fmt::Debug for Hooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hooks")
            .field("change", &self.change.is_some())
            .field("commit", &self.commit.is_some())
            .field("rollback", &self.rollback.is_some())
            .finish()
    }
}

/// How to open the database and set up each pooled connection.
#[derive(Debug, Clone)]
pub struct SqliteConfig {
    /// Where the database lives.
    pub location: Location,
    /// Read-write / read-only.
    pub mode: OpenMode,
    /// Journal mode for a file database (`None`: leave the file's own).
    pub journal_mode: Option<JournalMode>,
    /// How long a statement waits for another connection's lock before
    /// failing with "database is locked".
    pub busy_timeout: Duration,
    /// Enforce `FOREIGN KEY` constraints (off by default in SQLite itself).
    pub foreign_keys: bool,
    /// `PRAGMA synchronous`.
    pub synchronous: Synchronous,
    /// Extra `PRAGMA name = value` run on every new connection, in order.
    pub pragmas: Vec<(String, String)>,
    /// Rust functions registered on every connection.
    pub functions: Vec<ScalarFunction>,
    /// Connections per pool (per core).
    pub pool_size: usize,
    pub(crate) hooks: Hooks,
    /// Native extensions loaded into every connection: library path and
    /// entry point (`None`: SQLite's default name).
    pub(crate) extensions: Vec<(PathBuf, Option<String>)>,
    /// A database image to load into a fresh in-memory database.
    pub(crate) image: Option<Arc<Vec<u8>>>,
}

static NEXT_MEMORY: AtomicU64 = AtomicU64::new(0);

impl SqliteConfig {
    fn with_location(location: Location) -> Self {
        let file = matches!(location, Location::File(_));
        Self {
            location,
            mode: OpenMode::ReadWriteCreate,
            journal_mode: file.then_some(JournalMode::Wal),
            busy_timeout: Duration::from_secs(5),
            foreign_keys: true,
            synchronous: Synchronous::Normal,
            pragmas: Vec::new(),
            functions: Vec::new(),
            pool_size: 4,
            hooks: Hooks::default(),
            extensions: Vec::new(),
            image: None,
        }
    }

    /// A database file, created if missing, in WAL mode.
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::with_location(Location::File(path.into()))
    }

    /// A fresh private in-memory database.
    pub fn memory() -> Self {
        let n = NEXT_MEMORY.fetch_add(1, Ordering::Relaxed);
        Self::with_location(Location::Memory(format!("kernway-{}-{n}", std::process::id())))
    }

    /// Parse a URL: `sqlite::memory:`, `sqlite:relative/path.db`,
    /// `sqlite://relative/path.db`, `sqlite:///absolute/path.db`,
    /// `sqlite:///C:/data/app.db`, or a bare path — with optional
    /// `?mode=ro|rw|rwc&journal_mode=wal|delete|…&busy_timeout=<ms>&foreign_keys=on|off&synchronous=off|normal|full|extra&pool=<n>`.
    pub fn from_url(url: &str) -> Result<Self, OrmError> {
        let bad = |m: String| OrmError::Connection(format!("sqlite URL {url:?}: {m}"));
        let (base, query) = url.split_once('?').unwrap_or((url, ""));
        let path = base.strip_prefix("sqlite:").unwrap_or(base);
        let mut cfg = if matches!(path, ":memory:" | "") {
            Self::memory()
        } else {
            let p = if let Some(rest) = path.strip_prefix("//") {
                // `sqlite:///C:/x` → `C:/x`; `sqlite:///var/x` → `/var/x`.
                let drive = rest.len() > 3 && rest.as_bytes()[0] == b'/' && rest.as_bytes()[2] == b':' && rest.as_bytes()[1].is_ascii_alphabetic();
                if drive { &rest[1..] } else { rest }
            } else {
                path
            };
            Self::file(p)
        };
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| bad(format!("`{pair}` has no value")))?;
            let v_lower = v.to_ascii_lowercase();
            match k {
                "mode" => {
                    cfg.mode = match v_lower.as_str() {
                        "ro" => OpenMode::ReadOnly,
                        "rw" => OpenMode::ReadWrite,
                        "rwc" => OpenMode::ReadWriteCreate,
                        _ => return Err(bad(format!("mode must be ro, rw or rwc, not {v}"))),
                    }
                }
                "journal_mode" => {
                    cfg.journal_mode = Some(match v_lower.as_str() {
                        "wal" => JournalMode::Wal,
                        "delete" => JournalMode::Delete,
                        "truncate" => JournalMode::Truncate,
                        "persist" => JournalMode::Persist,
                        "memory" => JournalMode::Memory,
                        "off" => JournalMode::Off,
                        _ => return Err(bad(format!("unknown journal_mode {v}"))),
                    })
                }
                "busy_timeout" => {
                    let ms: u64 = v.parse().map_err(|_| bad(format!("busy_timeout must be milliseconds, not {v}")))?;
                    cfg.busy_timeout = Duration::from_millis(ms);
                }
                "foreign_keys" => {
                    cfg.foreign_keys = match v_lower.as_str() {
                        "on" | "true" | "1" => true,
                        "off" | "false" | "0" => false,
                        _ => return Err(bad(format!("foreign_keys must be on or off, not {v}"))),
                    }
                }
                "synchronous" => {
                    cfg.synchronous = match v_lower.as_str() {
                        "off" => Synchronous::Off,
                        "normal" => Synchronous::Normal,
                        "full" => Synchronous::Full,
                        "extra" => Synchronous::Extra,
                        _ => return Err(bad(format!("unknown synchronous {v}"))),
                    }
                }
                "pool" => cfg.pool_size = v.parse().map_err(|_| bad(format!("pool must be a number, not {v}")))?,
                _ => return Err(bad(format!("unknown parameter `{k}`"))),
            }
        }
        Ok(cfg)
    }

    /// Open read-only.
    pub fn read_only(mut self) -> Self {
        self.mode = OpenMode::ReadOnly;
        self
    }

    /// Set the busy timeout.
    pub fn busy_timeout(mut self, t: Duration) -> Self {
        self.busy_timeout = t;
        self
    }

    /// Set the journal mode.
    pub fn journal_mode(mut self, m: JournalMode) -> Self {
        self.journal_mode = Some(m);
        self
    }

    /// Run `PRAGMA name = value` on every new connection.
    pub fn pragma(mut self, name: &str, value: &str) -> Self {
        self.pragmas.push((name.to_string(), value.to_string()));
        self
    }

    /// Connections per pool.
    pub fn pool_size(mut self, n: usize) -> Self {
        self.pool_size = n.max(1);
        self
    }

    /// Make `name(args…)` callable from SQL, computed by `body` (`args = -1`
    /// for any number of arguments). `deterministic` functions (same input →
    /// same output) may be used in indexes and are optimised by SQLite.
    pub fn function(
        mut self,
        name: &str,
        args: i32,
        deterministic: bool,
        body: impl Fn(&[Value]) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        self.functions.push(ScalarFunction { name: name.to_string(), args, deterministic, body: Arc::new(body) });
        self
    }

    /// Call `f` for every row inserted, updated or deleted through any of the
    /// driver's connections (SQLite's update hook). It runs on the
    /// connection's thread while the statement runs, so keep it quick —
    /// typically push to a channel. Changes of a transaction that is later
    /// rolled back are reported too; pair with [`on_rollback`](Self::on_rollback).
    pub fn on_change(mut self, f: impl Fn(&Change) + Send + Sync + 'static) -> Self {
        self.hooks.change = Some(Arc::new(f));
        self
    }

    /// Call `f` after each commit (explicit, or a statement's autocommit).
    pub fn on_commit(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.hooks.commit = Some(Arc::new(f));
        self
    }

    /// Call `f` after each rollback.
    pub fn on_rollback(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.hooks.rollback = Some(Arc::new(f));
        self
    }

    /// Load the SQLite extension library at `path` (`.so` / `.dylib` /
    /// `.dll`; SQLite also tries the platform suffix when it is left out)
    /// into every connection, calling `entry_point` (default: SQLite's
    /// `sqlite3_extension_init` / `sqlite3_<name>_init` rule) — e.g.
    /// `sqlite-vec` for vector search or SpatiaLite.
    ///
    /// Extension loading stays switched off for SQL: `SELECT
    /// load_extension(…)` in a query is refused, so only the application
    /// chooses what is loaded. A library that fails to load fails the open.
    ///
    /// # Safety
    ///
    /// The library is native code run inside this process with full access to
    /// it: load only extensions you trust, from a path an attacker cannot
    /// write to.
    pub unsafe fn extension(mut self, path: impl Into<PathBuf>, entry_point: Option<&str>) -> Self {
        self.extensions.push((path.into(), entry_point.map(str::to_string)));
        self
    }

    /// The SQL run on a new connection: pragmas in a fixed order.
    pub(crate) fn setup_sql(&self) -> Vec<String> {
        let mut out = vec![format!("PRAGMA foreign_keys = {}", if self.foreign_keys { "ON" } else { "OFF" })];
        if self.mode != OpenMode::ReadOnly {
            if let (Location::File(_), Some(j)) = (&self.location, self.journal_mode) {
                out.push(format!("PRAGMA journal_mode = {}", j.sql()));
            }
            out.push(format!("PRAGMA synchronous = {}", self.synchronous.sql()));
        }
        out.extend(self.pragmas.iter().map(|(k, v)| format!("PRAGMA {k} = {v}")));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert!(matches!(SqliteConfig::from_url("sqlite::memory:").unwrap().location, Location::Memory(_)));
        assert!(matches!(SqliteConfig::from_url(":memory:").unwrap().location, Location::Memory(_)));
        assert_eq!(SqliteConfig::from_url("sqlite:data/app.db").unwrap().location, Location::File("data/app.db".into()));
        assert_eq!(SqliteConfig::from_url("sqlite://data/app.db").unwrap().location, Location::File("data/app.db".into()));
        assert_eq!(SqliteConfig::from_url("sqlite:///var/db/app.db").unwrap().location, Location::File("/var/db/app.db".into()));
        assert_eq!(SqliteConfig::from_url("sqlite:///C:/data/app.db").unwrap().location, Location::File("C:/data/app.db".into()));
        assert_eq!(SqliteConfig::from_url("app.db").unwrap().location, Location::File("app.db".into()));
        let c = SqliteConfig::from_url("sqlite:x.db?mode=ro&busy_timeout=250&foreign_keys=off&journal_mode=delete&synchronous=full&pool=2").unwrap();
        assert_eq!(c.mode, OpenMode::ReadOnly);
        assert_eq!(c.busy_timeout, Duration::from_millis(250));
        assert!(!c.foreign_keys);
        assert_eq!(c.journal_mode, Some(JournalMode::Delete));
        assert_eq!(c.synchronous, Synchronous::Full);
        assert_eq!(c.pool_size, 2);
        for bad in ["x.db?mode=zz", "x.db?nope=1", "x.db?busy_timeout=soon", "x.db?flag"] {
            assert!(SqliteConfig::from_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn two_memory_databases_are_distinct() {
        assert_ne!(SqliteConfig::memory().location, SqliteConfig::memory().location);
    }

    #[test]
    fn setup_skips_writes_when_read_only() {
        let rw = SqliteConfig::file("x.db").setup_sql();
        assert!(rw.contains(&"PRAGMA journal_mode = WAL".to_string()));
        let ro = SqliteConfig::file("x.db").read_only().setup_sql();
        assert!(!ro.iter().any(|s| s.contains("journal_mode") || s.contains("synchronous")));
        assert!(!SqliteConfig::memory().setup_sql().iter().any(|s| s.contains("journal_mode")));
    }
}
