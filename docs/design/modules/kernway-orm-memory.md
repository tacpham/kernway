# kernway-orm-memory — In-memory `Repository<T>` for tests and prototyping

## Purpose

A `HashMap`-backed implementation of `kernway-orm-core`'s `Repository<T>` and
`QueryBuilder<T>` traits. Its only role is to replace a real database in unit
tests and early prototyping — you can exercise service code without a file, a
connection, or a schema.

**Not** a production backend. Data is stored in process memory and lost when
the value is dropped. There are no transactions, no durability, and no
concurrent-access guarantees beyond a single `Mutex` (no multi-writer safety).

## Status

| Area | State | Notes |
|---|---|---|
| `Repository<T>` — all 9 CRUD methods | ✅ done | find_by_id, find_all, find_all_by_ids, count, exists_by_id, save, save_all, delete_by_id, delete_all_by_ids |
| Auto-increment ID assignment | ✅ done | AtomicU64 counter; assigns if `entity.id() == T::Id::default()` |
| `QueryBuilder<T>` — all 5 filters | ✅ done | filter_eq, filter_ne, filter_gt, filter_lt, filter_like |
| `QueryBuilder<T>` — ordering | ✅ done | order_by_asc, order_by_desc (last wins — single sort key) |
| `QueryBuilder<T>` — pagination | ✅ done | limit, offset, fetch_page |
| `QueryBuilder::with` (eager load) | 🚧 no-op | accepted but silently ignored — relations not supported |
| Transactions | ❌ not started | no rollback; save_all is a loop of individual saves |
| Multi-key ordering | ❌ not started | only the last `order_by_*` call is kept |

**Today**: a full drop-in for `Repository<T>` in tests. Every CRUD method and
every filter works.
**Not yet**: `.with()` (relation loading), multi-sort, transactions, snapshots.

## Standards

No external spec. The surface must satisfy `kernway-orm-core`'s `Repository<T>`
and `QueryBuilder<T>` contracts exactly — any divergence is a bug.

## Architecture

```text
InMemoryRepository<T>
    │
    ├─ store: Arc<Mutex<HashMap<T::Id, T>>>
    │         shared between the repository and every MemoryQueryBuilder it vends
    │
    └─ next_id: AtomicU64
               used only when entity.id() == T::Id::default() at save time

Repository<T>::query()
    └─► MemoryQueryBuilder<T>
            │  holds Arc clone of store + Vec<Filter> + Option<Order> + limit + offset
            │
            ├─ filter_* / order_by_* / limit / offset → push to builder (no I/O)
            │
            └─ terminal (fetch_all / fetch_one / fetch_count / fetch_page)
                    │
                    ├─ lock Mutex, clone all values
                    ├─ apply filters (field values via serde_json intermediary)
                    ├─ sort (if ordered)
                    └─ apply window (skip + take)
```

**Filtering via serde_json**: fields are addressed by name at runtime. There is
no static field accessor, so the entity is round-tripped through
`serde_json::to_value` to extract a field string, then compared. The comparison
is string-based for `Eq`/`Ne`/`Like` and numeric-aware for `Gt`/`Lt`. This is
the main design tradeoff: it works on any struct, but it is O(n × fields) and
not as fast as direct field access would be.

The same serde round-trip is used for ID assignment: when auto-increment fires,
the entity is serialised to a JSON object, the PK field is replaced, and the
object is deserialised back into `T`.

## Public surface

```rust
pub struct InMemoryRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned;

impl<T: …> InMemoryRepository<T> {
    /// Create an empty, isolated store.
    pub fn new() -> Self;
}

impl<T: …> Default for InMemoryRepository<T> { … }
impl<T: …> Repository<T> for InMemoryRepository<T> { … }
```

**Stability**: `InMemoryRepository::new()` is stable. The internal `store` and
`next_id` fields are private. `MemoryQueryBuilder` is not exported; it is only
reachable through `repo.query()`.

## Integration

**Depends on**:

| Module | Why |
|---|---|
| `kernway-orm-core` | `Entity`, `Repository`, `QueryBuilder`, `OrmError`, `Page` |
| `serde` / `serde_json` | field access by name and ID assignment without a proc-macro |

**Depended on by**:

| Module | What it uses |
|---|---|
| `kernway-orm-sqlite` (tests) | swapped in to verify service logic without a real DB |
| any crate under test | `InMemoryRepository::new()` in place of a real backend |

**Must never depend on**: `kernway-server`, `kernway-di`, or any runtime crate.
The whole point is that persistence can be tested in isolation.

## Speed

This backend is for tests. Speed in production is not a goal.

| Path | Runs | Current | Budget | Bench |
|---|---|---|---|---|
| `find_by_id` | per test assertion | O(1) HashMap lookup | — | none |
| `query().filter_eq().fetch_all` | per test | O(n × fields) — full scan + serde round-trip per entity | — | none |

**Allocation policy**: each query allocates a `Vec<T>` of cloned entities.
Acceptable for tests; not acceptable for production.

## Generic — extension points

| Extension point | Trait | Default impl | Replaceable by |
|---|---|---|---|
| `Repository<T>` | `kernway-orm-core::Repository` | `InMemoryRepository<T>` | any struct implementing the trait |
| `QueryBuilder<T>` | `kernway-orm-core::QueryBuilder` | `MemoryQueryBuilder<T>` | returned by the repo; not replaceable independently |

## Security

| Threat | Mitigation | Tested |
|---|---|---|
| Mutex poisoning (panic in a lock holder) | all lock calls map poison to `OrmError::Transaction` | no explicit test — error path coverage gap |
| Data shared across test threads leaking state | `InMemoryRepository::new()` creates an isolated store; each test should use its own instance | by convention — no enforcement |

## Direction

| Phase | Goal | Blocked by |
|---|---|---|
| Nice to have | Multi-key ordering (sort by field A then field B) | small feature, low priority |
| Nice to have | `with()` emitting a warning instead of silently doing nothing | needs a tracing integration or a return type change |
| Not planned | Transactions / snapshot / rollback | contradicts the stated scope; out of scope |

**Out of scope**: any form of persistence. If you need a test-scoped durable
store, `kernway-orm-sqlite` with `:memory:` is the right choice.

## Open questions

- Should `filter_like`'s `%` and `_` wildcards be interpreted (SQL semantics)
  or is the current `str::contains` check the intended behaviour? Right now
  `Like { pattern: "%foo%" }` matches the literal string `%foo%`, not any
  string containing `foo`.

## Related KEPs

| KEP | Bearing on this module |
|---|---|
| KEP-0000 §1 | why this crate depends on serde rather than on a Kernway-internal serialiser |
