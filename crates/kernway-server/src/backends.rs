//! Config-driven backend selection for the visitor/security stores.
//!
//! An app should not hard-code `FileBackedBans::open(...)`; it should say *what* it
//! wants in `kernway.properties` and let the framework build it — the same shape as
//! choosing the server address or the log level from config. Each store reads a
//! `kernway.<store>.store` key: `memory` (the default, pure in-memory), `file`
//! (durable on local disk, feature `persist`), or `redis` (shared, feature `redis`).
//!
//! ```properties
//! # a single-node app that keeps its bans and sessions across a restart, no server
//! kernway.persist.dir = data          # <dir>/bans, <dir>/sessions, <dir>/activity
//! kernway.persist.fsync = every-write # every-write | batched | never
//! kernway.bans.store = file
//! kernway.session.store = file
//! kernway.activity.store = memory
//! ```
//!
//! The `memory` arm is always available; `file`/`redis` compile in only with their
//! feature. Asking for a backend whose feature is off is a clear startup error, not a
//! silent fall-back to memory. These are a convenience — the raw
//! `FileBackedBans::open` / `RedisSessionStore::new` constructors remain, for an app
//! that wants full control (KEP-0000: paved road, never walled in).

use std::io;
use std::net::IpAddr;

use kernway_config::Config;
use kernway_security::session::{MemorySessionStore, SessionStore};
use kernway_security::Bans;

/// A backend construction or runtime error, unified across the memory/file/redis
/// stores so an admin handler sees one error type.
#[derive(Debug)]
pub struct BackendError(pub String);

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "backend error: {}", self.0)
    }
}

impl std::error::Error for BackendError {}

#[cfg(feature = "persist")]
impl From<kernway_security::PersistError> for BackendError {
    fn from(e: kernway_security::PersistError) -> Self {
        BackendError(e.0)
    }
}

#[cfg(feature = "redis")]
impl From<kernway_security::session::StoreError> for BackendError {
    fn from(e: kernway_security::session::StoreError) -> Self {
        BackendError(e.to_string())
    }
}

/// The ban list backend chosen by config. Hand [`bans`](Self::bans) to the
/// `BanFilter`, register this as a bean, and ban/unban through it — the call goes to
/// memory only, memory + local disk, or memory + Redis, per config.
pub enum BanBackend {
    /// Pure in-memory (lost on restart).
    Memory(Bans),
    /// Durable on local disk (feature `persist`).
    #[cfg(feature = "persist")]
    File(kernway_security::FileBackedBans),
    /// Durable + shared over Redis (feature `redis`).
    #[cfg(feature = "redis")]
    Redis(kernway_security::PersistentBans),
}

impl BanBackend {
    /// Build the backend named by `kernway.bans.store` (default `memory`).
    pub fn from_config(config: &Config) -> io::Result<Self> {
        match config.get_str("kernway.bans.store").unwrap_or("memory") {
            "memory" => Ok(Self::Memory(Bans::new())),
            #[cfg(feature = "persist")]
            "file" => Ok(Self::File(kernway_security::FileBackedBans::open(
                persist_path(config, "bans"),
                fsync_policy(config),
            )?)),
            #[cfg(feature = "redis")]
            "redis" => {
                let store = kernway_security::RedisBanStore::new(redis_address(config)?);
                // Preload the durable list into memory once, at startup, so the
                // per-request check stays in memory. Blocking here is fine — it is
                // assembly time, before any request is served. Two error layers to
                // unwrap: the executor's, then the restore's own StoreError.
                let bans = rt_core::Executor::new()
                    .map_err(|e| io::Error::other(e.to_string()))?
                    .block_on(kernway_security::PersistentBans::restore(store))
                    .map_err(|e| io::Error::other(e.to_string()))? // executor
                    .map_err(|e| io::Error::other(e.to_string()))?; // restore
                Ok(Self::Redis(bans))
            }
            other => Err(unavailable("kernway.bans.store", other)),
        }
    }

    /// The in-memory handle to give the `BanFilter` for the per-request check.
    #[must_use]
    pub fn bans(&self) -> Bans {
        match self {
            Self::Memory(bans) => bans.clone(),
            #[cfg(feature = "persist")]
            Self::File(store) => store.bans(),
            #[cfg(feature = "redis")]
            Self::Redis(store) => store.bans(),
        }
    }

    /// Ban an IP (in memory, and durably if the backend is file/redis).
    pub async fn ban_ip(&self, ip: IpAddr) -> Result<(), BackendError> {
        match self {
            Self::Memory(bans) => {
                bans.ban_ip(ip);
                Ok(())
            }
            #[cfg(feature = "persist")]
            Self::File(store) => store.ban_ip(ip).await.map_err(BackendError::from),
            #[cfg(feature = "redis")]
            Self::Redis(store) => store.ban_ip(ip).await.map_err(BackendError::from),
        }
    }

    /// Unban an IP.
    pub async fn unban_ip(&self, ip: IpAddr) -> Result<(), BackendError> {
        match self {
            Self::Memory(bans) => {
                bans.unban_ip(ip);
                Ok(())
            }
            #[cfg(feature = "persist")]
            Self::File(store) => store.unban_ip(ip).await.map_err(BackendError::from),
            #[cfg(feature = "redis")]
            Self::Redis(store) => store.unban_ip(ip).await.map_err(BackendError::from),
        }
    }

    /// Ban a subnet (CIDR).
    pub async fn ban_subnet(&self, cidr: &str) -> Result<(), BackendError> {
        match self {
            Self::Memory(bans) => {
                bans.ban_subnet(cidr);
                Ok(())
            }
            #[cfg(feature = "persist")]
            Self::File(store) => store.ban_subnet(cidr).await.map_err(BackendError::from),
            #[cfg(feature = "redis")]
            Self::Redis(store) => store.ban_subnet(cidr).await.map_err(BackendError::from),
        }
    }

    /// Unban a subnet.
    pub async fn unban_subnet(&self, cidr: &str) -> Result<(), BackendError> {
        match self {
            Self::Memory(bans) => {
                bans.unban_subnet(cidr);
                Ok(())
            }
            #[cfg(feature = "persist")]
            Self::File(store) => store.unban_subnet(cidr).await.map_err(BackendError::from),
            #[cfg(feature = "redis")]
            Self::Redis(store) => store.unban_subnet(cidr).await.map_err(BackendError::from),
        }
    }

    /// Ban a User-Agent phrase (case-insensitive).
    pub async fn ban_user_agent_containing(&self, phrase: &str) -> Result<(), BackendError> {
        match self {
            Self::Memory(bans) => {
                bans.ban_user_agent_containing(phrase);
                Ok(())
            }
            #[cfg(feature = "persist")]
            Self::File(store) => store.ban_user_agent_containing(phrase).await.map_err(BackendError::from),
            #[cfg(feature = "redis")]
            Self::Redis(store) => store.ban_user_agent_containing(phrase).await.map_err(BackendError::from),
        }
    }

    /// Unban a User-Agent phrase.
    pub async fn unban_user_agent_containing(&self, phrase: &str) -> Result<(), BackendError> {
        match self {
            Self::Memory(bans) => {
                bans.unban_user_agent_containing(phrase);
                Ok(())
            }
            #[cfg(feature = "persist")]
            Self::File(store) => store.unban_user_agent_containing(phrase).await.map_err(BackendError::from),
            #[cfg(feature = "redis")]
            Self::Redis(store) => store.unban_user_agent_containing(phrase).await.map_err(BackendError::from),
        }
    }
}

/// Build the [`SessionStore`] named by `kernway.session.store` (default `memory`), as
/// a `Box<dyn SessionStore>` to hand straight to a `SessionManager`.
pub fn session_store_from_config(config: &Config) -> io::Result<Box<dyn SessionStore>> {
    match config.get_str("kernway.session.store").unwrap_or("memory") {
        "memory" => Ok(Box::new(MemorySessionStore::new())),
        #[cfg(feature = "persist")]
        "file" => Ok(Box::new(kernway_security::FileBackedSessionStore::open(
            persist_path(config, "sessions"),
            fsync_policy(config),
        )?)),
        #[cfg(feature = "redis")]
        "redis" => {
            let ttl = std::time::Duration::from_secs(config.get_or("kernway.session.ttl-secs", 3600u64));
            Ok(Box::new(kernway_security::RedisSessionStore::new(redis_address(config)?, ttl)))
        }
        other => Err(unavailable("kernway.session.store", other)),
    }
}

/// Build the [`Activity`](kernway_security::activity::Activity) store named by
/// `kernway.activity.store` (default `memory`). The window is
/// `kernway.activity.window-secs` (default 300).
#[cfg(feature = "presence")]
pub fn activity_from_config(config: &Config) -> io::Result<std::sync::Arc<dyn kernway_security::Activity>> {
    let window = std::time::Duration::from_secs(config.get_or("kernway.activity.window-secs", 300u64));
    match config.get_str("kernway.activity.store").unwrap_or("memory") {
        "memory" => Ok(std::sync::Arc::new(kernway_security::InMemoryActivity::new(window))),
        #[cfg(feature = "persist")]
        "file" => Ok(std::sync::Arc::new(kernway_security::FileBackedActivity::open(
            persist_path(config, "activity"),
            window,
            fsync_policy(config),
        )?)),
        #[cfg(feature = "redis")]
        "redis" => Ok(std::sync::Arc::new(kernway_security::RedisActivity::new(redis_address(config)?, window))),
        other => Err(unavailable("kernway.activity.store", other)),
    }
}

/// A `<store>` value naming a backend that this build cannot provide (an unknown name,
/// or a `file`/`redis` value without its feature) — a clear error beats silently
/// falling back to memory.
fn unavailable(key: &str, value: &str) -> io::Error {
    io::Error::other(format!(
        "{key} = {value:?} is not available — expected \"memory\", \"file\" (feature `persist`), or \"redis\" (feature `redis`)"
    ))
}

/// `<kernway.persist.dir>/<store>` — the directory for a file-backed store.
#[cfg(feature = "persist")]
fn persist_path(config: &Config, store: &str) -> std::path::PathBuf {
    let base = config.get_str("kernway.persist.dir").unwrap_or("data");
    std::path::Path::new(base).join(store)
}

/// The `fsync` policy from `kernway.persist.fsync` (default `every-write`).
#[cfg(feature = "persist")]
fn fsync_policy(config: &Config) -> kernway_security::Fsync {
    match config.get_str("kernway.persist.fsync").unwrap_or("every-write") {
        "batched" => {
            let ms = config.get_or("kernway.persist.fsync-interval-ms", 1000u64);
            kernway_security::Fsync::Batched(std::time::Duration::from_millis(ms))
        }
        "never" => kernway_security::Fsync::Never,
        _ => kernway_security::Fsync::EveryWrite,
    }
}

/// The Redis address from `kernway.redis.address` (default `127.0.0.1:6379`).
#[cfg(feature = "redis")]
fn redis_address(config: &Config) -> io::Result<std::net::SocketAddr> {
    config
        .get_str("kernway.redis.address")
        .unwrap_or("127.0.0.1:6379")
        .parse()
        .map_err(|_| io::Error::other("kernway.redis.address must be host:port"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config built from inline `key=value` lines (the properties the loader parses).
    fn config(props: &str) -> Config {
        kernway_config::ConfigBuilder::default().parse(props).build()
    }

    #[test]
    fn defaults_to_pure_in_memory() {
        let c = config("");
        assert!(matches!(BanBackend::from_config(&c).unwrap(), BanBackend::Memory(_)));
        // The Box<dyn> ones just need to build; memory is the default.
        assert!(session_store_from_config(&c).is_ok());
    }

    #[test]
    fn an_explicit_memory_choice_is_honoured() {
        let c = config("kernway.bans.store=memory\nkernway.session.store=memory");
        assert!(matches!(BanBackend::from_config(&c).unwrap(), BanBackend::Memory(_)));
        assert!(session_store_from_config(&c).is_ok());
    }

    #[test]
    fn an_unknown_backend_is_a_clear_error() {
        let c = config("kernway.bans.store=postgres");
        // Avoid unwrap_err (would need BanBackend: Debug — it wraps non-Debug stores).
        let err = match BanBackend::from_config(&c) {
            Ok(_) => panic!("an unknown backend must not build"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("postgres") && err.contains("memory"), "helpful message: {err}");
    }

    #[cfg(not(feature = "persist"))]
    #[test]
    fn asking_for_file_without_the_feature_errors_not_silently_memory() {
        let c = config("kernway.session.store=file");
        assert!(session_store_from_config(&c).is_err(), "must not silently fall back to memory");
    }

    #[cfg(feature = "persist")]
    #[test]
    fn a_file_backend_is_built_and_is_durable() {
        let dir = std::env::temp_dir().join("kernway-backends-cfg");
        let _ = std::fs::remove_dir_all(&dir);
        let props = format!("kernway.bans.store=file\nkernway.persist.dir={}", dir.display());
        let backend = BanBackend::from_config(&config(&props)).unwrap();
        assert!(matches!(backend, BanBackend::File(_)));
        assert!(dir.join("bans").exists(), "the store's directory was created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "persist")]
    #[test]
    fn fsync_policy_parses() {
        assert!(matches!(fsync_policy(&config("")), kernway_security::Fsync::EveryWrite));
        assert!(matches!(fsync_policy(&config("kernway.persist.fsync=never")), kernway_security::Fsync::Never));
        assert!(matches!(
            fsync_policy(&config("kernway.persist.fsync=batched\nkernway.persist.fsync-interval-ms=500")),
            kernway_security::Fsync::Batched(_)
        ));
    }
}
