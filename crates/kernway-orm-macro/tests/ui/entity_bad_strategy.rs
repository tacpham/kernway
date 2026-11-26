use kernway_orm_macro::entity;

#[derive(Debug, Clone)]
#[entity]
struct Bad {
    #[id(strategy = "uuid")]
    id: u64,
}

fn main() {}
