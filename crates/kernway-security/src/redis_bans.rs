//! Redis-backed persistence for the ban list (feature = `redis`).
//!
//! The per-request ban check stays **in memory** ([`Bans`]) — a ban must not cost a
//! network round-trip on every request. Redis is the *durable, shared* copy: load it
//! into [`Bans`] at startup ([`RedisBanStore::load`]) so a restart keeps the bans, and
//! write each ban/unban through to Redis so a fresh instance picks them up on its own
//! startup. (Real-time propagation to *already-running* peers — pub/sub — is a further
//! step; startup-load already fixes "the ban list vanished on restart".)
//!
//! Each rule kind is a Redis SET, so ban/unban are idempotent `SADD`/`SREM`:
//! `kw:bans:ip`, `kw:bans:subnet`, `kw:bans:ua-exact`, `kw:bans:ua-contains`.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use kernway_redis::{Pool, RedisError};

use crate::session::StoreError;
use crate::tracking::{BanList, Bans};

const KEY_IP: &str = "kw:bans:ip";
const KEY_SUBNET: &str = "kw:bans:subnet";
const KEY_UA_EXACT: &str = "kw:bans:ua-exact";
const KEY_UA_CONTAINS: &str = "kw:bans:ua-contains";

/// The durable copy of the ban list. Mutations mirror what [`Bans`]
/// exposes in memory, so an admin handler updates both: the in-memory list for the
/// live check, and this store so the ban survives a restart.
pub struct RedisBanStore {
    pool: Pool,
}

/// Map the client's error to the backend-agnostic [`StoreError`].
fn to_store(err: RedisError) -> StoreError {
    StoreError::Backend(err.to_string())
}

impl RedisBanStore {
    /// A store talking to Redis at `addr`.
    #[must_use]
    pub fn new(addr: SocketAddr) -> Self {
        Self { pool: Pool::new(addr) }
    }

    /// Use a pre-configured [`Pool`] (e.g. one carrying `AUTH` credentials).
    #[must_use]
    pub fn from_pool(pool: Pool) -> Self {
        Self { pool }
    }

    /// Load the full ban list from Redis into an in-memory [`BanList`] — call at
    /// startup and hand the result to [`Bans::with`](crate::Bans::with).
    pub async fn load(&self) -> Result<BanList, StoreError> {
        let mut conn = self.pool.checkout().await.map_err(to_store)?;
        let ips = conn.smembers(KEY_IP).await.map_err(to_store)?;
        let subnets = conn.smembers(KEY_SUBNET).await.map_err(to_store)?;
        let ua_exact = conn.smembers(KEY_UA_EXACT).await.map_err(to_store)?;
        let ua_contains = conn.smembers(KEY_UA_CONTAINS).await.map_err(to_store)?;
        self.pool.checkin(conn);

        let mut list = BanList::new();
        for ip in &ips {
            if let Ok(ip) = ip.parse::<IpAddr>() {
                list.add_ip(ip);
            }
        }
        for cidr in &subnets {
            list.add_subnet(cidr);
        }
        for ua in &ua_exact {
            list.add_user_agent_exact(ua);
        }
        for phrase in &ua_contains {
            list.add_user_agent_containing(phrase);
        }
        Ok(list)
    }

    async fn add(&self, key: &str, member: &str) -> Result<(), StoreError> {
        let mut conn = self.pool.checkout().await.map_err(to_store)?;
        let result = conn.sadd(key, member).await.map_err(to_store);
        self.pool.checkin(conn);
        result
    }

    async fn remove(&self, key: &str, member: &str) -> Result<(), StoreError> {
        let mut conn = self.pool.checkout().await.map_err(to_store)?;
        let result = conn.srem(key, member).await.map_err(to_store);
        self.pool.checkin(conn);
        result
    }

    /// Persist an IP ban.
    pub async fn ban_ip(&self, ip: IpAddr) -> Result<(), StoreError> {
        self.add(KEY_IP, &ip.to_string()).await
    }

    /// Persist an IP unban.
    pub async fn unban_ip(&self, ip: IpAddr) -> Result<(), StoreError> {
        self.remove(KEY_IP, &ip.to_string()).await
    }

    /// Persist a subnet ban.
    pub async fn ban_subnet(&self, cidr: &str) -> Result<(), StoreError> {
        self.add(KEY_SUBNET, cidr).await
    }

    /// Persist a subnet unban.
    pub async fn unban_subnet(&self, cidr: &str) -> Result<(), StoreError> {
        self.remove(KEY_SUBNET, cidr).await
    }

    /// Persist an exact-User-Agent ban.
    pub async fn ban_user_agent_exact(&self, agent: &str) -> Result<(), StoreError> {
        self.add(KEY_UA_EXACT, agent).await
    }

    /// Persist a User-Agent-contains ban (stored lowercased, matching the in-memory rule).
    pub async fn ban_user_agent_containing(&self, phrase: &str) -> Result<(), StoreError> {
        self.add(KEY_UA_CONTAINS, &phrase.to_ascii_lowercase()).await
    }

    /// Persist a User-Agent-contains unban.
    pub async fn unban_user_agent_containing(&self, phrase: &str) -> Result<(), StoreError> {
        self.remove(KEY_UA_CONTAINS, &phrase.to_ascii_lowercase()).await
    }
}

/// A durable ban handle that keeps the in-memory [`Bans`] (the live per-request
/// check) and a [`RedisBanStore`] (the durable copy) in step. Restore it at startup,
/// give [`bans`](Self::bans) to the `BanFilter`, and register it as a bean so an admin
/// handler bans/unbans through it — each call updates memory *and* Redis, so the
/// live check is instant and the ban outlives a restart.
///
/// ```rust,ignore
/// let bans = PersistentBans::restore(RedisBanStore::new(addr)).await?;
/// app.layer(BanFilter::new(bans.bans())).register(bans.clone());
/// // in an admin handler: bans.ban_ip(addr).await?;
/// ```
#[derive(Clone)]
pub struct PersistentBans {
    bans: Bans,
    store: Arc<RedisBanStore>,
}

impl PersistentBans {
    /// Load the persisted bans into memory and return a handle over both.
    pub async fn restore(store: RedisBanStore) -> Result<Self, StoreError> {
        let list = store.load().await?;
        Ok(Self { bans: Bans::with(list), store: Arc::new(store) })
    }

    /// The in-memory list to hand to `BanFilter` for the per-request check.
    #[must_use]
    pub fn bans(&self) -> Bans {
        self.bans.clone()
    }

    /// Ban an IP in memory and in Redis.
    pub async fn ban_ip(&self, ip: IpAddr) -> Result<(), StoreError> {
        self.bans.ban_ip(ip);
        self.store.ban_ip(ip).await
    }

    /// Unban an IP in memory and in Redis.
    pub async fn unban_ip(&self, ip: IpAddr) -> Result<(), StoreError> {
        self.bans.unban_ip(ip);
        self.store.unban_ip(ip).await
    }

    /// Ban a subnet in memory and in Redis.
    pub async fn ban_subnet(&self, cidr: &str) -> Result<(), StoreError> {
        self.bans.ban_subnet(cidr);
        self.store.ban_subnet(cidr).await
    }

    /// Unban a subnet in memory and in Redis.
    pub async fn unban_subnet(&self, cidr: &str) -> Result<(), StoreError> {
        self.bans.unban_subnet(cidr);
        self.store.unban_subnet(cidr).await
    }

    /// Ban a User-Agent phrase in memory and in Redis.
    pub async fn ban_user_agent_containing(&self, phrase: &str) -> Result<(), StoreError> {
        self.bans.ban_user_agent_containing(phrase);
        self.store.ban_user_agent_containing(phrase).await
    }

    /// Unban a User-Agent phrase in memory and in Redis.
    pub async fn unban_user_agent_containing(&self, phrase: &str) -> Result<(), StoreError> {
        self.bans.unban_user_agent_containing(phrase);
        self.store.unban_user_agent_containing(phrase).await
    }
}
