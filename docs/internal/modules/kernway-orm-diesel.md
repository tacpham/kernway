# kernway-orm-diesel — ORM Reference Implementation

> Implementation of the `kernway-orm-core` spec using diesel + spawn_blocking.  
> This is the official implementation — it supports PostgreSQL, MySQL, and SQLite.

## Dependencies

```toml
[dependencies]
kernway-orm-core = { path = "../kernway-orm-core" }
diesel           = { version = "2", features = ["postgres", "mysql", "sqlite", "r2d2"] }
r2d2             = "0.8"
```

## User setup

```toml
# Cargo.toml
[dependencies]
kernway = { version = "0.4", features = ["orm-diesel", "db-postgres"] }
# hoặc: "db-mysql" / "db-sqlite"
```

```toml
# config/application.toml
[db]
url      = "${DATABASE_URL}"
max_size = 10
```

## Implementing `Repository` with Diesel

```rust
// kernway-orm-diesel/src/repository.rs

pub struct DieselRepository<T: Entity> {
    pool: Arc<r2d2::Pool<r2d2::ConnectionManager<PgConnection>>>,
    _phantom: PhantomData<T>,
}

impl<T: Entity + Queryable + Insertable> Repository<T> for DieselRepository<T> {
    async fn find_by_id(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        let pool = self.pool.clone();
        let id = id.clone();
        spawn_blocking(move || {
            let conn = pool.get().map_err(|e| OrmError::Connection(e.to_string()))?;
            T::table()
                .find(id)
                .first::<T>(&conn)
                .optional()
                .map_err(|e| OrmError::Query(e.to_string()))
        }).await
    }

    async fn save(&self, entity: T) -> Result<T, OrmError> {
        let pool = self.pool.clone();
        spawn_blocking(move || {
            let conn = pool.get()?;
            diesel::insert_into(T::table())
                .values(&entity)
                .on_conflict(T::id_column())
                .do_update()
                .set(&entity)
                .get_result::<T>(&conn)
                .map_err(map_diesel_error)
        }).await
    }

    // ... các methods còn lại
}

fn map_diesel_error(e: diesel::result::Error) -> OrmError {
    match e {
        diesel::result::Error::NotFound => OrmError::NotFound,
        diesel::result::Error::DatabaseError(
            diesel::result::DatabaseErrorKind::UniqueViolation, info
        ) => OrmError::UniqueViolation { field: info.column_name().unwrap_or("unknown").to_string() },
        _ => OrmError::Query(e.to_string()),
    }
}
```

## QueryBuilder Implementation

```rust
pub struct DieselQueryBuilder<T: Entity> {
    query: diesel::query_builder::BoxedSelectStatement<...>,
    withs: Vec<&'static str>,
    _phantom: PhantomData<T>,
}

impl<T: Entity> QueryBuilder<T> for DieselQueryBuilder<T> {
    fn filter(self: Box<Self>, predicate: Box<dyn EntityPredicate<T>>) -> Box<dyn QueryBuilder<T>> {
        // Convert predicate → diesel filter expression
        Box::new(DieselQueryBuilder {
            query: self.query.filter(predicate.to_diesel_expr()),
            ..(*self)
        })
    }

    fn limit(self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>> {
        Box::new(DieselQueryBuilder {
            query: self.query.limit(n as i64),
            ..(*self)
        })
    }

    async fn fetch_all(self: Box<Self>) -> Result<Vec<T>, OrmError> {
        let pool = self.pool.clone();
        spawn_blocking(move || {
            let conn = pool.get()?;
            self.query.load::<T>(&conn).map_err(map_diesel_error)
        }).await
    }

    async fn fetch_page(self: Box<Self>, page: u64, size: u64) -> Result<Page<T>, OrmError> {
        let total = self.clone().fetch_count().await?;
        let items = self.limit(size).offset(page * size).fetch_all().await?;
        Ok(Page {
            items,
            total,
            page,
            size,
            total_pages: (total + size - 1) / size,
        })
    }
}
```

## Relationship loading — avoiding N+1

```rust
// .with("posts") → JOIN users LEFT JOIN posts ON posts.user_id = users.id
// KHÔNG phải: load users rồi loop fetch posts từng cái

impl<T: Entity> QueryBuilder<T> for DieselQueryBuilder<T> {
    fn with(self: Box<Self>, relation: &'static str) -> Box<dyn QueryBuilder<T>> {
        Box::new(DieselQueryBuilder {
            withs: { let mut w = self.withs.clone(); w.push(relation); w },
            ..(*self)
        })
    }

    async fn fetch_all(self: Box<Self>) -> Result<Vec<T>, OrmError> {
        // Build JOIN query từ self.withs
        // Mỗi with → LEFT JOIN thêm vào query
        // 1 SQL query duy nhất, không phải N+1
    }
}
```