use kernway_orm_macro::entity;

#[derive(Debug, Clone)]
#[entity]
struct Pair(u64, String);

fn main() {}
