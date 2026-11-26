//! The MySQL connection pool: the shared per-core [`kernway_wire::Pool`],
//! opening connections with [`MySqlConnect`].

use crate::protocol::{Conn, MysqlConfig};
use kernway_orm_core::error::OrmError;
use kernway_wire::Connect;

/// Opens authenticated MySQL connections for the pool.
pub struct MySqlConnect(pub MysqlConfig);

impl Connect for MySqlConnect {
    type Conn = Conn;
    type Error = OrmError;

    async fn connect(&self) -> Result<Conn, OrmError> {
        Conn::connect(&self.0).await
    }
}

/// A per-core pool of MySQL connections.
pub type Pool = kernway_wire::Pool<MySqlConnect>;
/// A MySQL connection checked out of the [`Pool`].
pub type PooledConn = kernway_wire::Pooled<MySqlConnect>;
