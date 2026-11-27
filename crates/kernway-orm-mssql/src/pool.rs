//! The SQL Server connection pool: the shared per-core [`kernway_wire::Pool`].

use crate::config::MssqlConfig;
use crate::protocol::Conn;
use kernway_orm_core::error::OrmError;
use kernway_wire::Connect;

/// Opens logged-in SQL Server connections for the pool.
pub struct MssqlConnect(pub MssqlConfig);

impl Connect for MssqlConnect {
    type Conn = Conn;
    type Error = OrmError;

    async fn connect(&self) -> Result<Conn, OrmError> {
        Conn::connect(&self.0).await
    }
}
