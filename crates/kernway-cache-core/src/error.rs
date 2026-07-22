use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("deserialization error: {0}")]
    Deserialization(String),
    #[error("connection error: {0}")]
    Connection(String),
    #[error("operation error: {0}")]
    Operation(String),
    #[error("key too large: max {max} bytes, got {actual}")]
    KeyTooLarge { max: usize, actual: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_error_display_connection() {
        let e = CacheError::Connection("timeout".to_string());
        assert!(e.to_string().contains("connection error"));
    }

    #[test]
    fn cache_error_display_key_too_large() {
        let e = CacheError::KeyTooLarge { max: 256, actual: 512 };
        assert!(e.to_string().contains("256"));
        assert!(e.to_string().contains("512"));
    }
}
