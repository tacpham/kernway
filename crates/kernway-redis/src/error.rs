//! What can go wrong talking to Redis.

use std::fmt;

/// A Redis client error.
#[derive(Debug)]
pub enum RedisError {
    /// The socket failed (connect, read, or write).
    Io(std::io::Error),
    /// The server replied with an error (`-ERR …`) — the string is the message
    /// without the leading `-`.
    Server(String),
    /// The bytes on the wire were not valid RESP we understand.
    Protocol(String),
    /// A reply was well-formed but not the shape the command expected (e.g. an
    /// array where an integer was due).
    Unexpected(String),
    /// The connection was closed mid-reply (peer sent EOF), or is unusable
    /// after an earlier failure.
    Closed,
    /// A bad URL or option, or a setup step (resolve, read a certificate) failed.
    Config(String),
    /// The TLS handshake failed.
    Tls(String),
    /// No reply (or no connection) within the configured time; the
    /// connection is dropped.
    Timeout(String),
}

impl RedisError {
    /// Build an [`Unexpected`](RedisError::Unexpected) for a command that got a
    /// reply shape it did not expect.
    pub(crate) fn unexpected(command: &str, got: &crate::resp::Value) -> Self {
        RedisError::Unexpected(format!("{command}: unexpected reply {got:?}"))
    }
}

impl fmt::Display for RedisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RedisError::Io(e) => write!(f, "redis io: {e}"),
            RedisError::Server(m) => write!(f, "redis server error: {m}"),
            RedisError::Protocol(m) => write!(f, "redis protocol error: {m}"),
            RedisError::Unexpected(m) => write!(f, "redis unexpected reply: {m}"),
            RedisError::Closed => write!(f, "redis connection closed"),
            RedisError::Config(m) => write!(f, "redis config: {m}"),
            RedisError::Tls(m) => write!(f, "redis tls: {m}"),
            RedisError::Timeout(m) => write!(f, "redis timeout: {m}"),
        }
    }
}

impl std::error::Error for RedisError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RedisError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for RedisError {
    fn from(e: std::io::Error) -> Self {
        RedisError::Io(e)
    }
}
