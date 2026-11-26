# kernway-orm-sql — the shared SQL ORM layer and the backend contract

> Every relational database plugs into kernway through **one** layer. The ORM
> (repositories, query builder, transactions, CRUD SQL) is written once here;
> a database crate supplies only a wire driver and a small backend. A new
> database is not done until it passes the shared conformance suite.

## Layers

```
 user code ── kernway-orm-core traits (Driver, Repository, QueryBuilder, Transaction)
                         │
               kernway-orm-sql  (written once)
               ├─ engine        CRUD SQL: find / save / save_all / delete / by-id lists / FOR UPDATE
               ├─ SqlQueryBuilder  filters, Spec, order, limit/offset, page — all values bound
               ├─ SqlRepository    pooled autocommit repository
               ├─ run_transaction / SqlTx   begin → work → commit | rollback
               └─ conformance   the test suite every backend must pass
                         │  SqlConn + SqlDialect  (the contract)
        ┌────────────────┼──────────────────┐
  kernway-orm-mysql  kernway-orm-oracle  (next: postgres, mssql …)
  backend.rs ~100 l  backend.rs ~170 l
  wire protocol      wire protocol
                         │
       kernway-wire (BufStream, TLS upgrade, Pool<Connect>, StmtCache)
       kernway-orm-core::decode (Cell → entity, no per-cell String)
```

## What a backend implements

1. **A wire driver** — its own connection type (`Conn`) speaking the
   database protocol on the per-core reactor, built on `kernway-wire`
   (`BufStream`, `upgrade_tls`, `Pool`, `StmtCache`). No tokio, no C client.
2. **`SqlDialect`** (in `kernway-orm-core`) — quoting, placeholders
   (`?` / `$n` / `:n`), column types and DDL, `FOR UPDATE`, the row window
   (`limit_offset`), and database quirks the shared layer must know
   (`empty_string_is_null`).
3. **`SqlConn` for its `Conn`** — the only database-specific execution:

   | method | meaning |
   |---|---|
   | `select<T>(sql, params)` | run a query, decode rows into `T` via `kernway_orm_core::decode` |
   | `select_count(sql, params)` | first column of the first row |
   | `execute(sql, params)` | rows affected |
   | `insert_returning_id(sql, params, pk)` | INSERT, return the generated key (`LAST_INSERT_ID`, `RETURNING … INTO`, `RETURNING`, …) |
   | `insert_batch_first_id(sql, params, n)` | optional: multi-row INSERT reporting the first key of a consecutive range; `None` → row by row |
   | `begin` / `commit` / `rollback` | transaction control; outside a transaction every statement must be committed on its own |

   Values arrive as neutral `Param`s (`Null, Int, UInt, Float, Bool, Str,
   Bytes`); the backend converts them to its bind format.
4. **`Connect` + driver type** — `XxxConnect: kernway_wire::Connect`, and an
   `XxxDriver` whose `repository()` returns `SqlRepository<T, XxxConnect>`
   and whose `transaction()` calls `run_transaction`. Type aliases
   (`XxxRepository<T>`, `XxxTx`) are the public names.

A backend must **not** contain its own query builder, repository,
transaction scope, CRUD SQL or entity ↔ value mapping. If it needs
behaviour the shared layer lacks, extend the shared layer (usually a new
`SqlDialect` hook with a default) so every backend gets it.

## The conformance suite

`kernway-orm-sql` feature `conformance` exports `conformance::run_all(harness)`.
Each backend has `tests/conformance.rs` implementing `Harness` (connect, run
raw DDL, dialect) and calling `run_all` against a live server. Scenarios:

| scenario | checks |
|---|---|
| `crud` | generated keys, find, update, count, exists, delete |
| `values` | i64 min/max, floats (±0, 1e-300, f64::MAX), bool, NULL, Unicode/emoji, quotes/backslashes, empty strings |
| `batch` | `save_all` of 50 rows: distinct keys, input order; by-id find/delete; empty id list |
| `query_builder` | every filter, `Spec` trees, ordering, limit, offset alone, limit+offset, pages, `fetch_one`, unknown field → error, injection attempt stays a value |
| `binary_and_long_text` | BLOB 0 B / 1 KB all byte values / 1 MB, optional binary, 3 KB text |
| `unique_violation` | duplicate unique value → `OrmError::UniqueViolation`; manual key saves update |
| `transactions` | commit, rollback (update + delete undone), autocommit visible to another connection |
| `row_lock_no_lost_update` | two concurrent `find_by_id_for_update` + save on different threads → no lost update |

Run:

```sh
KERNWAY_MYSQL_URL=mysql://kernway:kernway_pw@127.0.0.1:3307/kernway_test \
  cargo test -p kernway-orm-mysql --features mysql --test conformance
KERNWAY_ORACLE_HOST=127.0.0.1 cargo test -p kernway-orm-oracle --test conformance
```

Differences a database legitimately has are expressed through the dialect
and asserted accordingly (Oracle stores `''` and empty BLOBs as NULL: text
and binary columns are created nullable and read back empty).

## Adding a database — checklist

1. Crate `kernway-orm-<db>`: wire driver on `kernway-wire`, live tests of
   the protocol itself (types, errors, TLS, large values) against a real
   server — not only against another client library's output.
2. `XxxDialect: SqlDialect`; override only what differs.
3. `impl SqlConn for Conn` (`backend.rs`), `XxxConnect`, `XxxDriver`.
4. `tests/conformance.rs` → `run_all` passes against the live server.
5. Errors: map the database's unique-constraint error to
   `OrmError::UniqueViolation`, connection failures to
   `OrmError::Connection` (the pool discards those connections).
