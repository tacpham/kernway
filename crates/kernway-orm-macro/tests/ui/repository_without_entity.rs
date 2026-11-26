#![allow(async_fn_in_trait)]
use kernway_orm_macro::repository;

#[repository]
trait Repo {
    async fn find_by_name(&self, name: &str) -> Result<Vec<u8>, ()>;
}

fn main() {}
