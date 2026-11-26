use kernway_orm_macro::entity;

#[derive(Debug, Clone)]
#[entity]
struct Bad {
    #[id(generated = "yes")]
    id: u64,
}

fn main() {}
