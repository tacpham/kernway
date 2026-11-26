use kernway_orm_macro::entity;

#[derive(Debug, Clone)]
#[entity]
struct Post {
    #[id]
    id: u64,
    #[many_to_one(column = "user_id")]
    user: u64,
}

fn main() {}
