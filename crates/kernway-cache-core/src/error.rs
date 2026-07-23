use thiserror::Error;

/// Everything a cache operation can fail with.
///
/// Worth remembering: a cache failure is rarely fatal. The usual response is to
/// log it and fall through to the real source, not to fail the request.
#[derive(Debug, Error)]
pub enum CacheError {
    /// The value could not be encoded for storage.
    #[error("serialization error: {0}")]
    Serialization(String),
    /// A stored value could not be decoded — typically the shape changed while
    /// old entries were still live.
    #[error("deserialization error: {0}")]
    Deserialization(String),
    /// The cache backend was unreachable.
    #[error("connection error: {0}")]
    Connection(String),
    /// The backend rejected the operation itself.
    #[error("operation error: {0}")]
    Operation(String),
    /// The key exceeded the backend's limit.
    #[error("key too large: max {max} bytes, got {actual}")]
    KeyTooLarge {
        /// Largest key the backend accepts, in bytes.
        max: usize,
        /// Size of the key that was offered, in bytes.
        actual: usize,
    },
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
