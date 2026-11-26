//! Row <-> entity mapping. Decoding is the shared `kernway_orm_core::decode`;
//! entity values become bound parameters in the shared `kernway-orm-sql` layer.

pub use kernway_orm_core::decode::{cells_to_entity, row_to_entity};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MySqlDialect;
    use kernway_orm_core::{entity::Entity, SqlDialect};
    use kernway_orm_macro::entity;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "items")]
    struct Item {
        #[id(strategy = "auto")]
        id: u64,
        name: String,
        value: i32,
    }

    #[test]
    fn row_roundtrips_into_entity() {
        let row = [Some("7".to_string()), Some("alpha".to_string()), Some("10".to_string())];
        let item: Item = row_to_entity(&row, Item::columns()).unwrap();
        assert_eq!(item, Item { id: 7, name: "alpha".into(), value: 10 });
    }

    #[test]
    fn create_table_is_mysql_flavoured() {
        let ddl = MySqlDialect.create_table_sql(Item::table_name(), Item::columns());
        assert!(ddl.contains("`items`"), "{ddl}");
        assert!(ddl.contains("AUTO_INCREMENT"), "{ddl}");
        assert!(ddl.contains("PRIMARY KEY"), "{ddl}");
    }
}
