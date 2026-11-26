use kernway_orm_macro::entity;

#[derive(Debug, Clone)]
#[entity]
struct Bad {
    #[id]
    id: u64,
    #[column(length = 10)]
    name: String,
}

fn main() {}
