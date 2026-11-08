//! Redis-backed pub/sub implementing the backend-agnostic
//! [`kernway_cache_core::pubsub`] contract.
//!
//! `!Send` — a connection is pinned to its shard's reactor. Build a
//! [`RedisPubSub`] per shard from the cheap [`RedisConfig`]. Publishing reuses
//! one connection; each subscription owns a dedicated one (a subscribed Redis
//! connection cannot issue other commands).

use crate::conn::Connection;
use crate::error::RedisError;
use kernway_cache_core::pubsub::{BoxFuture, Message, Publisher, Subscriber, Subscription};
use kernway_cache_core::CacheError;
use std::cell::RefCell;
use std::net::SocketAddr;

fn to_cache_err(e: RedisError) -> CacheError {
    match e {
        RedisError::Io(e) => CacheError::Connection(e.to_string()),
        RedisError::Closed => CacheError::Connection("redis connection closed".into()),
        other => CacheError::Operation(other.to_string()),
    }
}

/// Connection parameters for a Redis endpoint.
#[derive(Debug, Clone)]
pub struct RedisConfig {
    /// Server address.
    pub addr: SocketAddr,
    /// ACL username (`None` for a password-only / default-user server).
    pub user: Option<String>,
    /// Password (`None` for an unauthenticated server — not recommended).
    pub password: Option<String>,
}

impl RedisConfig {
    /// Config with username + password auth (Redis 6+ ACL).
    pub fn with_auth(addr: SocketAddr, user: impl Into<String>, password: impl Into<String>) -> Self {
        Self { addr, user: Some(user.into()), password: Some(password.into()) }
    }
}

/// Redis-backed [`PubSub`](kernway_cache_core::pubsub::PubSub).
pub struct RedisPubSub {
    config: RedisConfig,
    pub_conn: RefCell<Option<Connection>>,
}

impl RedisPubSub {
    /// Create a handle (connections open lazily on first use).
    pub fn new(config: RedisConfig) -> Self {
        Self { config, pub_conn: RefCell::new(None) }
    }

    /// Open and authenticate a fresh connection.
    async fn open(&self) -> Result<Connection, CacheError> {
        let mut conn = Connection::connect(self.config.addr).await.map_err(to_cache_err)?;
        if let Some(password) = &self.config.password {
            conn.auth(self.config.user.as_deref(), password).await.map_err(to_cache_err)?;
        }
        Ok(conn)
    }
}

impl Publisher for RedisPubSub {
    fn publish<'a>(&'a self, channel: &'a str, payload: &'a [u8]) -> BoxFuture<'a, Result<u64, CacheError>> {
        Box::pin(async move {
            // Take the pooled publish connection out (no RefCell borrow held
            // across the await), or open one. Return it only if it stayed healthy.
            let taken = self.pub_conn.borrow_mut().take();
            let mut conn = match taken {
                Some(c) => c,
                None => self.open().await?,
            };
            match conn.publish(channel, payload).await {
                Ok(n) => {
                    *self.pub_conn.borrow_mut() = Some(conn);
                    Ok(n as u64)
                }
                Err(e) => Err(to_cache_err(e)), // drop the broken connection
            }
        })
    }
}

impl Subscriber for RedisPubSub {
    fn subscribe<'a>(
        &'a self,
        channels: &'a [&'a str],
    ) -> BoxFuture<'a, Result<Box<dyn Subscription + 'a>, CacheError>> {
        Box::pin(async move {
            let mut conn = self.open().await?;
            conn.subscribe(channels).await.map_err(to_cache_err)?;
            Ok(Box::new(RedisSubscription { conn }) as Box<dyn Subscription>)
        })
    }
}

/// A live Redis subscription — owns its connection, unsubscribes on drop.
struct RedisSubscription {
    conn: Connection,
}

impl Subscription for RedisSubscription {
    fn recv(&mut self) -> BoxFuture<'_, Result<Message, CacheError>> {
        Box::pin(async move {
            let (channel, payload) = self.conn.next_message().await.map_err(to_cache_err)?;
            Ok(Message { channel, payload })
        })
    }
}
