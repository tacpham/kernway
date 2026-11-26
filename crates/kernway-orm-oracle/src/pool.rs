//! The Oracle connection pool: the shared per-core [`kernway_wire::Pool`],
//! opening connections with [`OracleConnect`].

use crate::conn::{Conn, OracleConfig};
use kernway_orm_core::error::OrmError;
use kernway_wire::Connect;

/// Opens authenticated Oracle sessions for the pool.
pub struct OracleConnect(pub OracleConfig);

impl Connect for OracleConnect {
    type Conn = Conn;
    type Error = OrmError;

    async fn connect(&self) -> Result<Conn, OrmError> {
        Conn::connect(&self.0).await
    }
}

/// A per-core pool of Oracle sessions.
pub type Pool = kernway_wire::Pool<OracleConnect>;
