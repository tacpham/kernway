use kernway_orm_macro::entity;

#[derive(Debug, Clone)]
#[entity]
struct NoKey {
    name: String,
}

fn main() {}
