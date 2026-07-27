# kernway-orm-macro — Proc-macro: `#[entity]` and field attributes

## Purpose

Generates `impl Entity for T` at compile time from a plain Rust struct.
The developer annotates fields; the macro emits the table name, primary key
accessor, and column metadata the backends (`orm-memory`, `orm-sqlite`, …)
need to build queries without reflection.

**Not** a runtime crate — it produces zero code that runs in production, only
`impl` blocks the compiler resolves. It does not depend on `kernway-orm-core`
at the crate level; it only emits token paths into that namespace, so the ORM
subsystem can be used without the rest of the framework.

## Status

| Area | State | Notes |
|---|---|---|
| `#[entity(table = "…")]` | ✅ done | snake_case default; `table = "..."` override |
| `#[id]` | ✅ done | exactly one per struct; compile error if missing or duplicated |
| `#[id(strategy = "auto")]` | ✅ done | sets `ColumnDef::auto = true`; backend handles INCREMENT |
| `#[column(name = "…")]` | ✅ done | column name override |
| `#[column(nullable)]` | ✅ done | inferred from `Option<T>`; overridable |
| `#[column(unique)]` | ✅ done | sets `ColumnDef::unique = true` |
| `#[repository]` | ❌ not started | planned — derive `find_by_*`, `exists_by_*`, `delete_by_*` from method names |
| Relationship attributes | ❌ not started | `#[one_to_many]`, `#[many_to_one]` — planned, no timeline |

**Today**: `#[entity]` + field attributes produce a working `impl Entity`.
**Not yet**: `#[repository]` derivation — callers implement the trait manually.

## Standards

No external spec. The design mirrors JPA annotations (`@Entity`, `@Id`,
`@Column`) but is not bound by JSR-338.

## Architecture

```text
#[entity(table = "users")]     ← attribute proc-macro entry point
struct User {
    #[id(strategy = "auto")]   ← parsed by parse_id_attr
    pub id: i64,
    #[column(name = "email")]  ← parsed by parse_column_attr
    pub email: String,
}

 compile time
       │
       ▼
 syn::ItemStruct parse
       │
       ├─ extract_table_name() → "users"
       ├─ for each field:
       │     is_id?     → id_field, id_type, auto flag
       │     #[column]? → column_name, nullable, unique
       │     else       → derive from field name + type
       │
       └─ quote! →
            impl ::kernway_orm_core::Entity for User {
                type Id = i64;
                fn table_name() -> &'static str { "users" }
                fn id(&self) -> &i64 { &self.id }
                fn columns() -> &'static [ColumnDef] {
                    static COLS: OnceLock<Vec<ColumnDef>> = OnceLock::new();
                    COLS.get_or_init(|| vec![ … ]).as_slice()
                }
            }
```

`columns()` uses `OnceLock` so the metadata is initialised once and returned
as a `&'static` slice on every subsequent call — zero allocations on the hot
path.

## Public surface

```rust
/// Maps a struct onto a database table.
/// table = "name" — defaults to snake_case of the struct name.
#[proc_macro_attribute]
pub fn entity(args: TokenStream, input: TokenStream) -> TokenStream

// Field attributes (inside #[entity]-annotated structs only):
// #[id]                         primary key, inferred type
// #[id(strategy = "auto")]      backend manages INCREMENT/SERIAL
// #[column]                     explicit mapping, name defaults to field name
// #[column(name = "col_name")]  column name override
// #[column(nullable)]           allow NULL (also inferred from Option<T>)
// #[column(unique)]             UNIQUE constraint hint
```

**Stability**: the attribute interface is considered stable. The generated
token paths (`::kernway_orm_core::…`) are stable as long as that crate's
public API is. Internal helpers (`extract_table_name`, `parse_id_attr`, …)
are not public.

## Integration

**Depends on**:

| Crate | Why |
|---|---|
| `syn` | parse the struct and its attributes |
| `quote` | emit the `impl Entity` token stream |
| `proc-macro2` | span-preserving tokens |

**Depended on by**:

| Module | What it uses |
|---|---|
| `kernway-orm-memory` | `#[entity]` on test structs |
| `kernway-orm-sqlite` | `#[entity]` on production entities |
| any crate using the ORM | `#[entity]` to register structs |

**Must never depend on**: `kernway-orm-core` as a crate (it only emits paths
into that namespace — a circular dependency would prevent standalone use).

## Speed

The macro runs at compile time only. No hot path in production.

| Path | Runs | Current | Budget | Bench |
|---|---|---|---|---|
| `#[entity]` expansion | once per annotated struct, compile time | not measured | < 1 ms per struct | none — irrelevant |

**Allocation policy**: zero allocations at runtime. The only allocation is
`OnceLock::get_or_init` on first call to `columns()`, which initialises a
`Vec<ColumnDef>` once and never again.

## Generic — extension points

| Extension point | Trait | Default impl | Replaceable by |
|---|---|---|---|
| Column type mapping | `column_type_tokens()` (private) | Rust primitive → `ColumnType` | not exposed — open a PR |

A user cannot add a custom `ColumnType` variant without modifying the crate.
This is a known limitation for exotic types (UUID, JSONB, arrays); tracked as
an open question below.

## Security

| Threat | Mitigation | Tested |
|---|---|---|
| Macro injection (crafted struct input) | `syn` parsing is safe; no `unsafe`; compile errors are type-checked | compiler rejects malformed input |
| Missing `#[id]` → runtime panic | rejected at compile time with a clear error | `#[entity]` requires exactly one `#[id]` — compile_fail tests |

## Direction

| Phase | Goal | Blocked by |
|---|---|---|
| Next | `#[repository]` — derive `find_by_field(val)` method signatures | design decision on return type (sync Result vs future) |
| Later | Custom column type hook — let a driver register extra `ColumnType` variants | needs a KEP (changes the spec crate's public enum) |
| Later | Relationship attributes (`#[one_to_many]`, `#[many_to_one]`) | requires the driver-side `.with()` to be implemented first |

**Out of scope**: runtime schema migration — that is `kernway-orm-sqlite` /
future `kernway-orm-sqlx`'s job. The macro only describes the mapping.

## Open questions

- Should `#[repository]` generate an `impl Repository<T>` or just a
  struct + method stubs? The latter is safer (user fills in the gaps) but
  defeats the Spring Data analogy.
- How should exotic types (UUID, JSONB, arrays) map to `ColumnType`? An
  extension hook or a wider enum?

## Related KEPs

| KEP | Bearing on this module |
|---|---|
| KEP-0000 §1 | why orm-macro emits paths but does not depend on orm-core |
