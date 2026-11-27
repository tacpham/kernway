//! The PostgreSQL connection pool: the shared per-core [`kernway_wire::Pool`],
//! opening connections with [`PgConnect`].

use crate::config::PgConfig;
use crate::protocol::Conn;
use kernway_orm_core::error::OrmError;
use kernway_wire::Connect;

/// Opens authenticated PostgreSQL connections for the pool.
pub struct PgConnect(pub PgConfig);

impl Connect for PgConnect {
    type Conn = Conn;
    type Error = OrmError;

    async fn connect(&self) -> Result<Conn, OrmError> {
        Conn::connect(&self.0).await
    }
}

/// A per-core pool of PostgreSQL connections.
pub type Pool = kernway_wire::Pool<PgConnect>;
/// A PostgreSQL connection checked out of the [`Pool`].
pub type PooledConn = kernway_wire::Pooled<PgConnect>;
