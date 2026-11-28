//! What `#[entity]` emits, read back through the `Entity` trait: table names,
//! column names / types / flags, single and composite keys, relations, and that
//! the struct itself is left intact (field attributes it does not own survive).

use kernway_orm_core::{ColumnType, Entity, RelationKind};
use kernway_orm_macro::entity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity]
struct OrderLineItem {
    #[id(strategy = "auto")]
    id: i64,
    #[column(name = "qty")]
    quantity: i32,
    #[column(unique)]
    sku: String,
    #[column(nullable = true)]
    note: String,
    discount: Option<f64>,
    #[column]
    shipped: bool,
    payload: Vec<u8>,
    tags: Vec<String>,
    small: u8,
    big: u64,
    #[serde(rename = "when")]
    created: Option<i64>,
}

fn col(name: &str) -> &'static kernway_orm_core::ColumnDef {
    OrderLineItem::columns().iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"))
}

#[test]
fn the_table_name_defaults_to_the_snake_cased_struct_name() {
    assert_eq!(OrderLineItem::table_name(), "order_line_item");
    assert_eq!(Renamed::table_name(), "people");
}

#[test]
fn every_field_becomes_a_column_in_declaration_order() {
    let names: Vec<_> = OrderLineItem::columns().iter().map(|c| (c.name, c.field)).collect();
    assert_eq!(
        names,
        [
            ("id", "id"),
            ("qty", "quantity"),
            ("sku", "sku"),
            ("note", "note"),
            ("discount", "discount"),
            ("shipped", "shipped"),
            ("payload", "payload"),
            ("tags", "tags"),
            ("small", "small"),
            ("big", "big"),
            ("created", "created"),
        ]
    );
}

#[test]
fn column_types_are_inferred_from_the_rust_type() {
    let ty = |n| col(n).col_type.clone();
    assert_eq!(ty("id"), ColumnType::BigInt);
    assert_eq!(ty("qty"), ColumnType::Integer);
    assert_eq!(ty("sku"), ColumnType::Text);
    assert_eq!(ty("discount"), ColumnType::Float, "Option<T> takes T's type");
    assert_eq!(ty("shipped"), ColumnType::Boolean);
    assert_eq!(ty("payload"), ColumnType::Binary, "Vec<u8> is binary");
    assert_eq!(ty("tags"), ColumnType::Unknown, "any other Vec is not a scalar column");
    assert_eq!(ty("small"), ColumnType::Integer);
    assert_eq!(ty("big"), ColumnType::BigInt);
    assert_eq!(ty("created"), ColumnType::BigInt);
}

#[test]
fn flags_follow_the_attributes_and_the_type() {
    let id = col("id");
    assert!(id.primary_key && id.unique && id.auto && !id.nullable, "{id:?}");
    let sku = col("sku");
    assert!(sku.unique && !sku.primary_key && !sku.auto && !sku.nullable);
    assert!(col("note").nullable, "nullable = true on a non-Option field");
    assert!(col("discount").nullable, "Option<T> is nullable");
    assert!(col("created").nullable);
    assert!(!col("qty").nullable && !col("qty").unique);
    // Only the #[id] column is a key.
    assert_eq!(OrderLineItem::columns().iter().filter(|c| c.primary_key).count(), 1);
}

#[test]
fn the_struct_keeps_its_fields_and_foreign_attributes() {
    let item = OrderLineItem {
        id: 9,
        quantity: 2,
        sku: "A-1".into(),
        note: String::new(),
        discount: None,
        shipped: false,
        payload: vec![1],
        tags: vec![],
        small: 1,
        big: 2,
        created: Some(5),
    };
    assert_eq!(item.id(), 9);
    // `#[serde(rename)]` was kept on the field: the macro only strips its own.
    let back: OrderLineItem = serde_json::from_value(serde_json::json!({
        "id": 9, "quantity": 2, "sku": "A-1", "note": "", "discount": null, "shipped": false,
        "payload": [1], "tags": [], "small": 1, "big": 2, "when": 5
    }))
    .unwrap();
    assert_eq!(back, item);
}

#[derive(Debug, Clone)]
#[entity(table = "people")]
struct Renamed {
    #[id]
    email: String,
}

#[test]
fn an_explicit_key_is_not_auto() {
    let id = &Renamed::columns()[0];
    assert!(id.primary_key && !id.auto);
    assert_eq!(Renamed { email: "a@b".into() }.id(), "a@b");
    assert!(Renamed::relations().is_empty());
}

#[derive(Debug, Clone)]
#[entity(table = "stock")]
struct Stock {
    #[id]
    warehouse: String,
    qty: i64,
    #[id]
    sku: u32,
}

#[test]
fn several_ids_form_a_tuple_key_in_declaration_order() {
    let s = Stock { warehouse: "WH1".into(), qty: 3, sku: 42 };
    let key: (String, u32) = s.id();
    assert_eq!(key, ("WH1".to_string(), 42));
    let keys: Vec<_> = Stock::columns().iter().filter(|c| c.primary_key).map(|c| c.name).collect();
    assert_eq!(keys, ["warehouse", "sku"]);
    assert_eq!(s.qty, 3);
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // only its metadata is under test
#[entity(table = "comments")]
struct Comment {
    #[id]
    id: u64,
    #[many_to_one(entity = "posts")]
    post_id: u64,
    #[many_to_one(entity = "users", column = "author")]
    written_by: u64,
}

#[test]
fn many_to_one_records_the_relation_and_keeps_the_column() {
    let rels = Comment::relations();
    assert_eq!(rels.len(), 2);
    assert_eq!((rels[0].field, rels[0].kind, rels[0].target_table, rels[0].foreign_key), ("post_id", RelationKind::ManyToOne, "posts", "post_id"));
    assert_eq!(rels[1].field, "written_by");
    assert_eq!(rels[1].foreign_key, "author", "column = … names the foreign key");
    assert_eq!(rels[1].join_table, None);
    let cols: Vec<_> = Comment::columns().iter().map(|c| c.name).collect();
    assert_eq!(cols, ["id", "post_id", "written_by"]);
}

#[test]
fn metadata_is_built_once_and_shared() {
    assert!(std::ptr::eq(OrderLineItem::columns(), OrderLineItem::columns()));
    assert!(std::ptr::eq(Comment::relations(), Comment::relations()));
}

#[derive(Debug, Clone)]
#[entity]
struct Assigned {
    #[id(strategy = "assigned")]
    code: String,
}

#[derive(Debug, Clone)]
#[entity]
struct Manual {
    #[id(strategy = "manual")]
    code: String,
}

#[test]
fn assigned_is_the_jpa_name_for_a_manual_key() {
    for id in [&Assigned::columns()[0], &Manual::columns()[0]] {
        assert!(id.primary_key && !id.auto, "{id:?}");
    }
}
