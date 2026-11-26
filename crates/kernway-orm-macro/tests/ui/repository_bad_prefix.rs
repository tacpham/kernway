#![allow(async_fn_in_trait)]
use kernway_orm_core::error::OrmError;
use kernway_orm_macro::{entity, repository};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[entity]
struct User {
    #[id]
    id: u64,
    name: String,
}

#[repository(entity = User)]
trait Repo {
    async fn load_by_name(&self, name: &str) -> Result<Vec<User>, OrmError>;
}

fn main() {}
