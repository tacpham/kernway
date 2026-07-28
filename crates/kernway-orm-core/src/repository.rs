use crate::{entity::Entity, error::OrmError, query::QueryBuilder, BoxFuture};

/// CRUD operations for a single Entity type.
/// Equivalent to `JpaRepository<T, ID>` in Spring Data JPA.
///
/// # Two-tier design: primitives + defaults
///
/// Like Spring Data's `SimpleJpaRepository`, this trait provides **reference
/// implementations** for most methods, so a driver only has to supply four
/// **primitives**:
///
/// - [`find_by_id`](Self::find_by_id)
/// - [`save`](Self::save)
/// - [`delete_by_id`](Self::delete_by_id)
/// - [`query`](Self::query) (the fluent [`QueryBuilder`] factory)
///
/// Everything else — [`find_all`](Self::find_all),
/// [`find_all_by_ids`](Self::find_all_by_ids), [`count`](Self::count),
/// [`exists_by_id`](Self::exists_by_id), [`save_all`](Self::save_all),
/// [`delete_all_by_ids`](Self::delete_all_by_ids) — has a default built on those
/// primitives. A driver **overrides** a default only when its backend offers a
/// cheaper path (a batch endpoint, an exact count, …). This is the ORM's
/// capability model: the spec gives correct-but-generic behaviour; each backend
/// upgrades what it does well and leaves the rest to the default.
pub trait Repository<T: Entity>: Send + Sync {
    // --- Primitives (a driver must implement these) ---

    /// Find by primary key. Returns `None` if not found.
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>>;

    /// Insert or update the entity, returning the stored form.
    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>>;

    /// Delete by primary key.
    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>>;

    /// Entry point for the fluent query builder.
    ///
    /// # Example
    /// ```rust,ignore
    /// repo.query()
    ///     .filter_eq("role", "ADMIN")
    ///     .order_by_desc("created_at")
    ///     .fetch_page(0, 20)
    /// ```
    fn query(&self) -> Box<dyn QueryBuilder<T>>;

    // --- Defaulted (override for a cheaper backend-native path) ---

    /// Load all records.
    ///
    /// Default: an unfiltered [`query`](Self::query). Override when the backend
    /// has a cheaper "scan the whole collection" path (e.g. paging a document
    /// endpoint instead of a search).
    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async move { self.query().fetch_all().await })
    }

    /// Load multiple records by their IDs.
    ///
    /// Default: one [`find_by_id`](Self::find_by_id) per id, skipping misses.
    /// Override when the backend has a batch / `IN` lookup.
    fn find_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(row) = self.find_by_id(id).await? {
                    out.push(row);
                }
            }
            Ok(out)
        })
    }

    /// Total record count.
    ///
    /// Default: `query().fetch_count()`. Override when the backend has an exact,
    /// cheaper counter.
    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        Box::pin(async move { self.query().fetch_count().await })
    }

    /// Check existence without materialising the entity.
    ///
    /// Default: `find_by_id(id).is_some()`. Override with a `HEAD` / `EXISTS`
    /// path when the backend has one.
    fn exists_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<bool, OrmError>> {
        Box::pin(async move { Ok(self.find_by_id(id).await?.is_some()) })
    }

    /// Save many entities, returning their stored forms.
    ///
    /// Default: one [`save`](Self::save) per entity. Override with a batch write
    /// when the backend supports it.
    fn save_all<'a>(&'a self, entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(entities.len());
            for entity in entities {
                out.push(self.save(entity).await?);
            }
            Ok(out)
        })
    }

    /// Delete many records by their IDs.
    ///
    /// Default: one [`delete_by_id`](Self::delete_by_id) per id. Override with a
    /// batch delete when the backend supports it.
    fn delete_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        Box::pin(async move {
            for id in ids {
                self.delete_by_id(id).await?;
            }
            Ok(())
        })
    }
}
