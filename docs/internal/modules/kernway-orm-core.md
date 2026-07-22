# kernway-orm-core — ORM Specification

> Pure spec — traits and annotations only. No implementation. Compile time < 1s.  
> Equivalent to JPA (JSR-338) in Java.  
> **Compatibility note**: see [kernway-orm-jpa-compat.md](kernway-orm-jpa-compat.md) — ~85% of JPA features, 15% redesigned around Rust idioms.

## Standards

- JSR-338 (JPA 2.2) — design inspiration (non-binding)
- SQL:2016 — query semantics
- RFC 7807 — error format for ORM errors

## Mandatory implementation rules

> **Required**: Every implementation (`kernway-orm-diesel`, `kernway-orm-sqlx`, ...) MUST fully implement all traits below. Do not add APIs outside the spec to the user-facing layer.

---

## Core Traits

### `Entity`

```rust
/// Marker trait cho ORM-managed struct.
/// Tương đương @Entity trong JPA.
pub trait Entity: Send + Sync + Sized + 'static {
    type Id: Send + Sync + Clone + Eq + std::hash::Hash;

    /// Tên bảng trong database.
    fn table_name() -> &'static str;

    /// Lấy primary key của entity.
    fn id(&self) -> &Self::Id;

    /// Metadata của tất cả columns.
    fn columns() -> &'static [ColumnDef];
}
```

### `Repository<T>`

```rust
/// CRUD operations cho một Entity type.
/// Tương đương JpaRepository<T, ID> trong Spring Data JPA.
pub trait Repository<T: Entity>: Send + Sync {
    // --- Read ---
    async fn find_by_id(&self, id: &T::Id)
        -> Result<Option<T>, OrmError>;

    async fn find_all(&self)
        -> Result<Vec<T>, OrmError>;

    async fn find_all_by_ids(&self, ids: &[T::Id])
        -> Result<Vec<T>, OrmError>;

    async fn count(&self)
        -> Result<u64, OrmError>;

    async fn exists_by_id(&self, id: &T::Id)
        -> Result<bool, OrmError>;

    // --- Write ---
    async fn save(&self, entity: T)
        -> Result<T, OrmError>;

    async fn save_all(&self, entities: Vec<T>)
        -> Result<Vec<T>, OrmError>;

    async fn delete_by_id(&self, id: &T::Id)
        -> Result<(), OrmError>;

    async fn delete_all_by_ids(&self, ids: &[T::Id])
        -> Result<(), OrmError>;

    // --- Query builder entry point ---
    fn query(&self) -> Box<dyn QueryBuilder<T>>;
}
```

### `QueryBuilder<T>`

```rust
/// Fluent query API.
/// Tương đương CriteriaBuilder + TypedQuery trong JPA.
pub trait QueryBuilder<T: Entity>: Send {
    /// Filter — WHERE clause.
    fn filter(self: Box<Self>, predicate: Box<dyn EntityPredicate<T>>)
        -> Box<dyn QueryBuilder<T>>;

    /// ORDER BY ASC
    fn order_by_asc(self: Box<Self>, field: &'static str)
        -> Box<dyn QueryBuilder<T>>;

    /// ORDER BY DESC
    fn order_by_desc(self: Box<Self>, field: &'static str)
        -> Box<dyn QueryBuilder<T>>;

    /// LIMIT
    fn limit(self: Box<Self>, n: u64)
        -> Box<dyn QueryBuilder<T>>;

    /// OFFSET
    fn offset(self: Box<Self>, n: u64)
        -> Box<dyn QueryBuilder<T>>;

    /// Eager load relationship — tránh N+1.
    fn with(self: Box<Self>, relation: &'static str)
        -> Box<dyn QueryBuilder<T>>;

    // --- Terminal operations ---
    async fn fetch_all(self: Box<Self>)
        -> Result<Vec<T>, OrmError>;

    async fn fetch_one(self: Box<Self>)
        -> Result<Option<T>, OrmError>;

    async fn fetch_count(self: Box<Self>)
        -> Result<u64, OrmError>;

    async fn fetch_page(self: Box<Self>, page: u64, size: u64)
        -> Result<Page<T>, OrmError>;
}
```

### `OrmTransaction`

```rust
/// Transaction context.
/// Tương đương EntityTransaction / @Transactional trong JPA.
/// Kernway's #[transactional] sử dụng trait này.
pub trait OrmTransaction: Send {
    async fn commit(self) -> Result<(), OrmError>;
    async fn rollback(self) -> Result<(), OrmError>;
    fn is_active(&self) -> bool;
}
```

---

## Annotations (macro specs)

### `#[entity]`

```rust
/// Map struct sang database table.
/// Tương đương @Entity + @Table trong JPA.
///
/// Tham số:
///   table = "table_name"   — tên bảng (mặc định: snake_case của struct name)
///
/// Generated:
///   impl Entity for MyStruct { ... }
#[proc_macro_attribute]
pub fn entity(args: TokenStream, input: TokenStream) -> TokenStream { ... }
```

### Field annotations

```rust
#[id]                          // PRIMARY KEY — bắt buộc có 1 field
#[id(strategy = "auto")]       // AUTO_INCREMENT / SERIAL
#[id(strategy = "uuid")]       // UUID v4 tự sinh

#[column]                      // map field → column (tên mặc định = field name)
#[column(name = "col_name")]   // custom column name
#[column(nullable = false)]    // NOT NULL
#[column(unique)]              // UNIQUE constraint
#[column(default = "value")]   // DEFAULT value
#[column(auto)]                // auto-managed (created_at, updated_at)

#[one_to_many(mapped_by = "foreign_key_field")]
#[many_to_one(column = "foreign_key_column")]
#[many_to_many(join_table = "table", join_column = "col", inverse_column = "col")]
```

### `#[repository]`

```rust
/// Sinh CRUD methods + method-name query generation.
/// Tương đương @Repository + extends JpaRepository<T, ID> trong Spring Data.
///
/// Auto-generated methods từ field names:
///   find_by_{field}(val)
///   find_by_{field}_and_{field}(val1, val2)
///   find_by_{field}_or_{field}(val1, val2)
///   delete_by_{field}(val)
///   exists_by_{field}(val)
///   count_by_{field}(val)
#[proc_macro_attribute]
pub fn repository(args: TokenStream, input: TokenStream) -> TokenStream { ... }
```

---

## Error Types

```rust
#[derive(Debug, thiserror::Error)]
pub enum OrmError {
    #[error("record not found")]
    NotFound,

    #[error("unique constraint violation: {field}")]
    UniqueViolation { field: String },

    #[error("foreign key violation")]
    ForeignKeyViolation,

    #[error("connection error: {0}")]
    Connection(String),

    #[error("query error: {0}")]
    Query(String),

    #[error("transaction error: {0}")]
    Transaction(String),

    #[error("type conversion error: {0}")]
    TypeConversion(String),
}
```

---

## Page type

```rust
pub struct Page<T> {
    pub items:       Vec<T>,
    pub total:       u64,
    pub page:        u64,
    pub size:        u64,
    pub total_pages: u64,
}
```

---

## Implementation Registry

Existing / in-progress implementations:

| Crate | Database | Async | Status |
|---|---|---|---|
| `kernway-orm-diesel` | PostgreSQL, MySQL, SQLite | spawn_blocking | Official — v0.4 |
| `kernway-orm-sqlx` | PostgreSQL, MySQL, SQLite | Native async | Community |
| `kernway-orm-mongodb` | MongoDB | Native async | Community |

---

## Rules for implementations

1. Fully implement **all** methods in `Repository<T>` and `QueryBuilder<T>`
2. Do not add public APIs outside the spec — extensions must go through `KernwayPlugin`
3. `OrmError` must be fully mapped from database-specific errors
4. `fetch_page` must use `LIMIT + OFFSET` or keyset pagination — do not load everything and then slice
5. N+1: `.with(relation)` MUST use JOINs or batch queries — not a lazy loop
