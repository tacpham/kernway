# kernway-orm-sqlite

SQLite for the kernway ORM: the shared SQL layer (`kernway-orm-sql`) over an
embedded SQLite. Acceptance report: [`docs/acceptance/sqlite.md`](../../acceptance/sqlite.md)
(`acceptance/run.sh sqlite`).

## Shape

```
SqliteDriver ──derefs──▶ SqlDb<SqliteConnect>      (shared: repositories, select builder,
     │                         │                     transactions, cursors, raw SQL, execute_many)
     │                    kernway_wire::Pool  (per core)
     │                         │
     │                    SqliteConn ── implements SqlConn
     │                         │   send job ─────────▶ connection thread
     │                         │   await reply ◀────── (rusqlite::Connection, open cursors)
     └── blob_* / serialize / backup_to / from_bytes  (SQLite-only, same path)
```

- **Async without blocking the reactor.** SQLite is a library: every call does
  file I/O and may wait on another connection's lock. Each pooled connection
  owns a dedicated thread holding its `rusqlite::Connection`; the async side
  sends a job and awaits a one-shot reply. A dropped future interrupts the
  running statement (`sqlite3_interrupt`), so a timeout frees the connection
  at once.
- **Cursors** are prepared statements kept on the connection thread and
  stepped a batch at a time (`raw_query` continues where the last batch
  stopped). `query_each` streams through one; nothing is materialised.
- **Concurrency.** One writer at a time. Transactions start `BEGIN IMMEDIATE`
  so read-modify-write transactions queue within the busy timeout (default
  5 s) instead of failing on their first write; files default to WAL so readers
  never wait. There is no `FOR UPDATE`: the dialect's lock suffix is empty and
  IMMEDIATE gives the same no-lost-update guarantee.
- **Types.** SQLite stores NULL / integer / real / text / blob; values come back
  typed by the column's declared type (`BOOLEAN`, `DATE`, `DATETIME`,
  `TIMESTAMP`, `TIME`, `JSON`, `DECIMAL`/`NUMERIC`), which the dialect emits for
  entity columns. `u64` beyond `i64::MAX` is refused, not rounded.
- **In-memory databases** use SQLite's `memdb` VFS, so every connection of one
  driver shares one database; an anchor connection keeps it alive.
- **Same engine everywhere.** `rusqlite` with `bundled`: SQLite compiled from
  source on Linux, macOS and Windows, FTS5 and the JSON functions included.

## Configuration

`SqliteConfig::file(path)` / `::memory()` / `::from_url(url)` — mode
(read-write-create, read-write, read-only), journal mode, busy timeout,
foreign keys (on by default), `synchronous`, extra PRAGMAs, pool size, Rust
scalar functions callable from SQL, and change / commit / rollback hooks.

URL forms: `sqlite::memory:`, `sqlite:relative.db`, `sqlite://relative.db`,
`sqlite:///abs/path.db`, `sqlite:///C:/data/app.db`, a bare path; options
`?mode=ro|rw|rwc&journal_mode=…&busy_timeout=<ms>&foreign_keys=on|off&synchronous=…&pool=<n>`.

## SQLite-only features

- **Change capture** (session extension): `track_changes(tables, |tx| …)` runs a
  transaction and returns what it changed as a `Changeset` (bytes to ship);
  `apply_changeset(cs, OnConflict::{Abort, Skip, Replace})` replays it
  elsewhere; `invert`, `concat`, `operations`; `track_patch` for patchsets.
  The session lives on the transaction's connection
  (`kernway_orm_sql::run_transaction_on`); an abandoned capture is dropped when
  that connection next begins a transaction.
- **Native extensions**: `unsafe SqliteConfig::extension(path, entry_point)`
  loads into every connection; SQL's own `load_extension()` stays refused.
  Proven with `tests/ext`, a real extension the test builds (a separate
  workspace, so rusqlite's `loadable_extension` feature never reaches the
  driver).
- Incremental BLOB I/O, change / commit / rollback hooks, serialize /
  `from_bytes`, online backup, Rust scalar functions, FTS5, JSON.

## Not available

- Stored procedures: SQLite has none — `call()` is `Unsupported`; register a
  Rust function and call it from SQL or with `call_function`.
