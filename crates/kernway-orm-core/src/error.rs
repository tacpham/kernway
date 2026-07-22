use thiserror::Error;

#[derive(Debug, Error)]
pub enum OrmError {
    #[error("record not found")]
    NotFound,
    #[error("unique constraint violation on field: {field}")]
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
