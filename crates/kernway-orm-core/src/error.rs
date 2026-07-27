use thiserror::Error;

/// Everything an ORM operation can fail with.
///
/// Backends map their native error types onto these variants, so calling code
/// distinguishes "duplicate email" from "database is down" without knowing
/// which driver is underneath.
#[derive(Debug, Error)]
pub enum OrmError {
    /// The row does not exist. Note that `find_by_id` returns `Ok(None)` for a
    /// simple miss — this variant is for operations that require a row.
    #[error("record not found")]
    NotFound,
    /// A UNIQUE constraint rejected the write. Usually a 409 to the client.
    #[error("unique constraint violation on field: {field}")]
    UniqueViolation {
        /// The column whose constraint was violated.
        field: String,
    },
    /// A foreign key constraint rejected the write — the referenced row is
    /// missing, or still referenced by others.
    #[error("foreign key violation")]
    ForeignKeyViolation,
    /// Could not obtain or use a connection: pool exhausted, network down,
    /// credentials refused.
    #[error("connection error: {0}")]
    Connection(String),
    /// The statement failed — bad SQL, a type mismatch, a missing column.
    #[error("query error: {0}")]
    Query(String),
    /// A transaction could not begin, commit, or roll back. Also covers a
    /// poisoned lock in the single-process backends.
    #[error("transaction error: {0}")]
    Transaction(String),
    /// A stored value would not convert into the Rust field type — the schema
    /// and the entity have drifted apart.
    #[error("type conversion error: {0}")]
    TypeConversion(String),
    /// A schema migration failed.
    #[error("migration error: {0}")]
    Migration(String),
    /// The backend does not implement this operation. Lets a driver decline a
    /// feature honestly instead of failing in some subtler way.
    #[error("driver not supported: {0}")]
    Unsupported(String),
}

#[cfg(test)]
mod tests {
    use super::OrmError;

    #[test]
    fn orm_error_not_found_display() {
        assert_eq!(OrmError::NotFound.to_string(), "record not found");
    }

    #[test]
    fn orm_error_unique_violation_display() {
        let err = OrmError::UniqueViolation {
            field: "email".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "unique constraint violation on field: email"
        );
    }
}
