# kernway-orm-sqlite — SQLite `Repository<T>` via rusqlite

## Purpose

A SQLite-backed implementation of `kernway-orm-core`'s `Repository<T>` and
`QueryBuilder<T>` traits. It is the first production-capable persistence driver
in the ORM stack: actual data survives process restarts (file mode), DDL is
generated automatically from `Entity` metadata, and the full CRUD + query
surface is available.

**Not** an async driver. All operations block on the calling thread. In
Kernway's thread-per-core model the caller decides where that blocking belongs
(typically `spawn_blocking`) — the driver itself stays dependency-free of any
runtime.

## Status

| Area | State | Notes |
|---|---|---|
| `Repository<T>` — all 9 CRUD methods | ✅ done | full impl |
| Auto-DDL (`CREATE TABLE IF NOT EXISTS`) | ✅ done | generated from `Entity::columns()` at construction |
| `#[id(strategy = "auto")]` | ✅ done | `AUTOINCREMENT`; returns entity with DB-assigned id after insert |
| WAL mode + foreign keys | ✅ done | `PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;` on open |
| `SqliteRepository::open(path)` | ✅ done | file-backed |
| `SqliteRepository::in_memory()` | ✅ done | private in-memory DB for tests (`:memory:` semantics) |
| `execute_raw(sql)` | ✅ done | escape hatch for fixtures / schema tweaks |
| `QueryBuilder<T>` — all 5 filters | ✅ done | compiled to parameterised `WHERE` clause |
| `QueryBuilder<T>` — ordering | ✅ done | `ORDER BY … ASC/DESC`; last `order_by_*` wins (single key) |
| `QueryBuilder<T>` — pagination | ✅ done | `LIMIT` / `OFFSET` / `fetch_page` |
| `QueryBuilder::with` (eager load) | 🚧 no-op | accepted but silently ignored |
| `UniqueViolation` / `ForeignKeyViolation` error mapping | ✅ done | rusqlite codes → `OrmError` variants |
| Migrations | ❌ not started | no schema versioning; DDL is append-only CREATE IF NOT EXISTS |
| Connection pooling | ❌ not started | single `Arc<Mutex<Connection>>` per repo instance |
| Multi-key ordering | ❌ not started | last `order_by_*` wins |
| Transactions across multiple saves | ❌ not started | `save_all` is a loop of individual writes |

**Today**: a working SQLite backend — open a file (or `:memory:`), call CRUD
and query methods, get back typed `T`. Suitable for single-process applications.
**Not yet**: connection pooling, migrations, multi-sort, eager relations, async.

## Standards

No formal spec. Compliance goals:

| Spec | Scope | Compliance |
|---|---|---|
| SQLite WAL mode | concurrency model | enabled on every connection |
| SQLite foreign key enforcement | referential integrity | enabled on every connection (`PRAGMA foreign_keys=ON`) |
| `kernway-orm-core` `Repository<T>` contract | CRUD semantics | full — all 9 methods implemented |
| `kernway-orm-core` `QueryBuilder<T>` contract | filter / order / page | full — all methods; `.with()` no-op |

## Architecture

```text
SqliteRepository<T>
    │
    ├─ conn: Arc<Mutex<Connection>>   ← shared with every SqliteQueryBuilder it vends
    │         WAL mode, FK enforcement enabled at open time
    │         DDL auto-created for T at construction
    │
    └─ _marker: PhantomData<T>

Entity → serde_json::Value → rusqlite params    (write path)
rusqlite Row → serde_json::Value → Entity       (read path)

─── Write path (save/insert/update) ───────────────────────────────────────
T::columns()              → column list, INSERT/UPDATE SQL template
serde_json::to_value(T)   → JSON object
json_to_sql(col, value)   → rusqlite SqlValue per column
  Boolean  → INTEGER 0/1
  Integer  → INTEGER (i64); rejects u64 > i64::MAX explicitly
  Float    → REAL
  Text/…   → TEXT; JSON columns stored as serialised string
conn.execute(sql, params) → rowid
(auto pk) re-SELECT by rowid → deserialise back into T with assigned id

─── Read path (find_by_id / find_all / query terminal) ─────────────────────
rusqlite Row  (column index i, ColumnDef)
    → SqlValue → sql_to_json(v, col) → serde_json::Value
    → serde_json::from_value::<T>()

─── Filter path (QueryBuilder) ─────────────────────────────────────────────
.filter_eq("role", "ADMIN") → accumulate in SqliteQueryBuilder.filters
.fetch_all() terminal → build WHERE clause from filters → parameterised query
  filter_value_for_field: parses string value to correct SqlValue type
  (INTEGER for boolean/integer columns, REAL for float, TEXT otherwise)
```

The serde_json intermediary is a deliberate tradeoff: it avoids a custom
`FromRow` derive and works with any `Serialize + DeserializeOwned` struct.
The cost is an extra allocation per row on every read. For an embedded SQLite
driver this is acceptable; a high-throughput server should prefer `sqlx` or
a zero-copy row mapper.

## Public surface

```rust
pub struct SqliteRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned + Default + PartialEq;

impl<T: …> SqliteRepository<T> {
    /// Open (or create) a file-backed SQLite database.
    /// Creates T's table on first open.
    pub fn open(path: &str) -> Result<Self, OrmError>;

    /// Open a private in-memory database. Data is lost when the repo is dropped.
    /// The usual choice for tests requiring real SQL (vs InMemoryRepository).
    pub fn in_memory() -> Result<Self, OrmError>;

    /// Run arbitrary SQL against the connection.
    /// Bypasses the Repository abstraction — not portable to another backend.
    pub fn execute_raw(&self, sql: &str) -> Result<(), OrmError>;
}

impl<T: …> Repository<T> for SqliteRepository<T> { … }
```

**Stability**: `open`, `in_memory`, and `execute_raw` are stable. The
`SqliteQueryBuilder` type is not exported; it is only reachable through
`repo.query()`. The serde_json intermediary is an implementation detail.

## Integration

**Depends on**:

| Module | Why |
|---|---|
| `kernway-orm-core` | `Entity`, `Repository`, `QueryBuilder`, `OrmError`, `Page`, `ColumnDef`, `ColumnType` |
| `rusqlite` | SQLite bindings |
| `serde` / `serde_json` | row marshaling intermediary |

**Depended on by**:

| Module | What it uses |
|---|---|
| any app wanting SQLite persistence | `SqliteRepository::open` / `in_memory` |
| `login-htmx` example (planned) | session / user storage |

**Must never depend on**: `kernway-server`, `kernway-di`, `kernway-security`,
or any Kernway runtime crate. Persistence must be usable without the web stack.

## Speed

| Path | Runs | Current | Budget | Bench |
|---|---|---|---|---|
| `save` (insert, no auto-PK) | per write | not measured | — | none |
| `save` (insert, auto-PK) | per write | 2 round-trips (INSERT + SELECT by rowid) | — | none |
| `find_by_id` | per request | not measured | — | none |
| `query().filter_eq().fetch_all` | per request | parameterised SQL — not measured | — | none |

Hypothesis (not measured): serde_json round-trip adds ~1–5 µs per row vs
direct row mapping. Acceptable for embedded SQLite; measure before optimising.

**Allocation policy**: each row allocates a `serde_json::Map` during
deserialisation. One `Vec<T>` per query result. No zero-copy path.

## Generic — extension points

| Extension point | Trait | Default impl | Replaceable by |
|---|---|---|---|
| `Repository<T>` backend | `kernway-orm-core::Repository` | `SqliteRepository<T>` | `kernway-orm-memory` for tests; future `kernway-orm-sqlx` for Postgres/MySQL |
| `QueryBuilder<T>` | `kernway-orm-core::QueryBuilder` | `SqliteQueryBuilder<T>` | not independently replaceable; comes from the repo |
| Row marshaling | internal `json_to_sql` / `sql_to_json` | serde_json intermediary | not exposed; open a PR |

## Security

| Threat | Mitigation | Tested |
|---|---|---|
| SQL injection | all filter values bound as `rusqlite` positional parameters; SQL is template-generated from `Entity` metadata (field names from proc-macro, not user input) | no explicit injection test — gap |
| Integer overflow (u64 > i64::MAX written to INTEGER column) | `number_to_integer` returns `OrmError::TypeConversion` rather than wrapping | logic covered by type conversion code; no separate test |
| Mutex poisoning | all lock calls map poison to `OrmError::Transaction` | no explicit test — error path gap |
| FOREIGN KEY violations silently ignored | `PRAGMA foreign_keys=ON` at open; `rusqlite::ErrorCode::ConstraintViolation` with FK message → `OrmError::ForeignKeyViolation` | no integration test with FK schema |

## Direction

| Phase | Goal | Blocked by |
|---|---|---|
| Next | Connection pooling (r2d2 or an internal pool) | design decision — one repo per pool or repo holds pool? |
| Next | Multi-key ordering | small change; low priority |
| Later | Schema migration (versioned ALTER TABLE) | needs a migration DSL or SQL file runner; separate sub-crate? |
| Later | `.with()` eager-load via JOIN or secondary SELECT | needs the relation model in `kernway-orm-core` first |
| Not planned | Async surface | sync stays sync; async callers use `spawn_blocking` |

**Out of scope**: Postgres, MySQL, MongoDB. Those are separate driver crates
(`kernway-orm-sqlx` for async PG/MySQL; community for Mongo). The sqlite driver
is explicitly for embedded / development use.

## Open questions

- Should `SqliteRepository` hold a connection pool instead of a single
  `Arc<Mutex<Connection>>`? A pool would avoid lock contention under concurrent
  writes at the cost of added complexity. SQLite WAL mode already allows
  concurrent reads; the single-connection model is the correct default for
  embedded use but needs revisiting if the driver is used in a multi-threaded
  server.
- The `execute_raw` escape hatch bypasses the `Repository` abstraction. Should
  it be gated behind a `#[cfg(test)]` attribute to prevent accidental production
  use?

## Related KEPs

| KEP | Bearing on this module |
|---|---|
| KEP-0000 §1 | why the driver uses rusqlite (ours) rather than sqlx (not yet) |
| KEP-0000 §2 | speed numbers above are hypotheses — bench before claiming |
