#![allow(async_fn_in_trait)]
use kernway_orm_core::error::OrmError;
use kernway_orm_macro::{entity, repository};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[entity]
struct User {
    #[id]
    id: u64,
    name: String,
    age: i64,
}

#[repository(entity = User)]
trait Repo {
    async fn find_by_name_and_age(&self, name: &str) -> Result<Vec<User>, OrmError>;
}

fn main() {}
