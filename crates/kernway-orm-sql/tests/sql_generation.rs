//! The SQL the shared layer generates, for the real dialect of every
//! backend — PostgreSQL, MySQL, SQLite, Oracle, SQL Server — against a
//! recording connection: golden statements for each dialect, and the rules
//! every dialect must keep (placeholders numbered in order, identifiers
//! quoted, row locks, row windows, batches, transactions).

use kernway_orm_core::{
    entity::{ColumnDef, ColumnType, Entity},
    error::OrmError,
    repository::Repository,
    spec::Spec,
    transaction::{Transaction, TransactionalDriver},
    SqlDialect,
};
use kernway_orm_macro::entity;
use kernway_orm_sql::{col, engine, Agg, Cmp, Outcome, Param, SqlConn, SqlDb, SqlRepository, Value};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

static DIALECTS: [(&str, &(dyn SqlDialect + Sync)); 5] = [
    ("postgres", &kernway_orm_postgres::PostgresDialect),
    ("mysql", &kernway_orm_mysql::MySqlDialect),
    ("sqlite", &kernway_orm_sqlite::SqliteDialect),
    ("oracle", &kernway_orm_oracle::OracleDialect),
    ("sqlserver", &kernway_orm_mssql::SqlServerDialect),
];

/// Run `$f::<D>()` for every dialect.
macro_rules! each_dialect {
    ($f:ident) => {{
        $f::<0>();
        $f::<1>();
        $f::<2>();
        $f::<3>();
        $f::<4>();
    }};
}

/// The expectation for dialect `D` out of one per dialect (in `DIALECTS` order).
fn golden<const D: usize>(per_dialect: [&str; 5]) -> &str {
    per_dialect[D]
}

// ---- a recording connection ------------------------------------------------

#[derive(Default)]
struct Log {
    statements: Vec<(String, Vec<Param>)>,
    rows: VecDeque<Vec<serde_json::Value>>,
    counts: VecDeque<u64>,
    batch_first_id: Option<u64>,
    next_id: i64,
}

#[derive(Clone)]
struct Recorder(Rc<RefCell<Log>>);

impl Recorder {
    fn new() -> Self {
        Recorder(Rc::new(RefCell::new(Log { next_id: 100, ..Default::default() })))
    }
    fn sql(&self) -> Vec<String> {
        self.0.borrow().statements.iter().map(|s| s.0.clone()).collect()
    }
    fn last(&self) -> (String, Vec<Param>) {
        self.0.borrow().statements.last().cloned().expect("a statement")
    }
    fn clear(&self) {
        self.0.borrow_mut().statements.clear();
    }
}

struct Fake<const D: usize> {
    log: Recorder,
}

impl<const D: usize> Fake<D> {
    fn record(&self, sql: &str, params: &[Param]) {
        self.log.0.borrow_mut().statements.push((sql.to_string(), params.to_vec()));
    }
}

impl<const D: usize> SqlConn for Fake<D> {
    fn dialect() -> &'static dyn SqlDialect {
        DIALECTS[D].1
    }
    async fn select<T: Entity + DeserializeOwned>(&mut self, sql: &str, params: &[Param]) -> Result<Vec<T>, OrmError> {
        self.record(sql, params);
        let rows = self.log.0.borrow_mut().rows.pop_front().unwrap_or_default();
        rows.into_iter().map(|r| serde_json::from_value(r).map_err(|e| OrmError::TypeConversion(e.to_string()))).collect()
    }
    async fn select_count(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        self.record(sql, params);
        Ok(self.log.0.borrow_mut().counts.pop_front().unwrap_or(0))
    }
    async fn execute(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        self.record(sql, params);
        Ok(1)
    }
    async fn insert_returning_id(&mut self, sql: &str, params: &[Param], _pk: &str) -> Result<Param, OrmError> {
        self.record(sql, params);
        let mut l = self.log.0.borrow_mut();
        l.next_id += 1;
        Ok(Param::Int(l.next_id))
    }
    async fn insert_batch_first_id(&mut self, sql: &str, params: &[Param], _rows: usize) -> Result<Option<u64>, OrmError> {
        let first = self.log.0.borrow().batch_first_id;
        if first.is_some() {
            self.record(sql, params);
        }
        Ok(first)
    }
    async fn begin(&mut self) -> Result<(), OrmError> {
        self.record("BEGIN", &[]);
        Ok(())
    }
    async fn commit(&mut self) -> Result<(), OrmError> {
        self.record("COMMIT", &[]);
        Ok(())
    }
    async fn rollback(&mut self) -> Result<(), OrmError> {
        self.record("ROLLBACK", &[]);
        Ok(())
    }
    async fn query_each<F>(&mut self, sql: &str, params: &[Param], _each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<Value>) -> Result<(), OrmError>,
    {
        self.record(sql, params);
        Ok(Outcome::default())
    }
}

struct FakeConnect<const D: usize>(Recorder);

impl<const D: usize> kernway_wire::Connect for FakeConnect<D> {
    type Conn = Fake<D>;
    type Error = OrmError;
    async fn connect(&self) -> Result<Fake<D>, OrmError> {
        Ok(Fake { log: self.0.clone() })
    }
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn conn<const D: usize>() -> (Fake<D>, Recorder) {
    let r = Recorder::new();
    (Fake { log: r.clone() }, r)
}

async fn repo<const D: usize>() -> (SqlRepository<Item, FakeConnect<D>>, SqlDb<FakeConnect<D>>, Recorder) {
    let log = Recorder::new();
    let db = SqlDb::open(FakeConnect::<D>(log.clone()), 2).await.unwrap();
    log.clear();
    (SqlRepository::new(db.pool().clone()), db, log)
}

/// Placeholder `i` (1-based) of dialect `D`.
fn ph<const D: usize>(i: usize) -> String {
    DIALECTS[D].1.placeholder(i)
}

/// Every placeholder the dialect numbers appears, in order, `n` of them.
fn assert_placeholders<const D: usize>(sql: &str, n: usize) {
    let d = DIALECTS[D].1;
    if d.placeholder(1) == d.placeholder(2) {
        assert_eq!(sql.matches(&d.placeholder(1)).count(), n, "{}: {sql}", DIALECTS[D].0);
        return;
    }
    let mut at = 0;
    for i in 1..=n {
        let p = d.placeholder(i);
        let found = sql[at..].find(&p).unwrap_or_else(|| panic!("{}: {p} missing or out of order in {sql}", DIALECTS[D].0));
        at += found + p.len();
    }
    assert!(!sql.contains(&d.placeholder(n + 1)), "{}: more than {n} placeholders in {sql}", DIALECTS[D].0);
}

// ---- entities -----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "items")]
struct Item {
    #[id(strategy = "auto")]
    id: i64,
    name: String,
    qty: i64,
    active: bool,
    note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "members")]
struct Member {
    #[id(strategy = "manual")]
    code: String,
    #[column(unique)]
    email: String,
    level: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "tags")]
struct Tag {
    #[id(strategy = "auto")]
    id: i64,
    item_id: i64,
    label: String,
}

fn item_json(id: i64, name: &str) -> serde_json::Value {
    serde_json::json!({ "id": id, "name": name, "qty": 1, "active": true, "note": null })
}

// ---- golden statements, per dialect ----------------------------------------------

fn crud_statements<const D: usize>() {
    block(async {
        let (mut c, log) = conn::<D>();
        engine::find_by_id::<Item, _>(&mut c, &7, false).await.unwrap();
        engine::find_by_id::<Item, _>(&mut c, &7, true).await.unwrap();
        log.0.borrow_mut().rows.push_back(vec![item_json(101, "a")]);
        let saved = engine::save(&mut c, Item { id: 0, name: "a".into(), qty: 1, active: true, note: None }).await.unwrap();
        assert_eq!(saved.id, 101, "the generated key is read back");
        engine::save(&mut c, Item { id: 5, name: "b".into(), qty: 2, active: false, note: None }).await.unwrap();
        engine::delete_by_id::<Member, _>(&mut c, &"A".to_string()).await.unwrap();
        let sql = log.sql();
        assert_eq!(
            sql[0],
            golden::<D>([
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" WHERE "id" = $1"#,
                "SELECT `id`, `name`, `qty`, `active`, `note` FROM `items` WHERE `id` = ?",
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" WHERE "id" = ?"#,
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" WHERE "id" = :1"#,
                "SELECT [id], [name], [qty], [active], [note] FROM [items] WHERE [id] = @P1",
            ])
        );
        assert_eq!(
            sql[1],
            golden::<D>([
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" WHERE "id" = $1 FOR UPDATE"#,
                "SELECT `id`, `name`, `qty`, `active`, `note` FROM `items` WHERE `id` = ? FOR UPDATE",
                // One writer at a time: no row locks to take.
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" WHERE "id" = ?"#,
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" WHERE "id" = :1 FOR UPDATE"#,
                "SELECT [id], [name], [qty], [active], [note] FROM [items] WITH (UPDLOCK, ROWLOCK) WHERE [id] = @P1",
            ])
        );
        assert_eq!(
            sql[2],
            golden::<D>([
                r#"INSERT INTO "items" ("name", "qty", "active", "note") VALUES ($1, $2, $3, $4)"#,
                "INSERT INTO `items` (`name`, `qty`, `active`, `note`) VALUES (?, ?, ?, ?)",
                r#"INSERT INTO "items" ("name", "qty", "active", "note") VALUES (?, ?, ?, ?)"#,
                r#"INSERT INTO "items" ("name", "qty", "active", "note") VALUES (:1, :2, :3, :4)"#,
                "INSERT INTO [items] ([name], [qty], [active], [note]) VALUES (@P1, @P2, @P3, @P4)",
            ])
        );
        assert_eq!(
            sql[4],
            golden::<D>([
                r#"UPDATE "items" SET "name" = $1, "qty" = $2, "active" = $3, "note" = $4 WHERE "id" = $5"#,
                "UPDATE `items` SET `name` = ?, `qty` = ?, `active` = ?, `note` = ? WHERE `id` = ?",
                r#"UPDATE "items" SET "name" = ?, "qty" = ?, "active" = ?, "note" = ? WHERE "id" = ?"#,
                r#"UPDATE "items" SET "name" = :1, "qty" = :2, "active" = :3, "note" = :4 WHERE "id" = :5"#,
                "UPDATE [items] SET [name] = @P1, [qty] = @P2, [active] = @P3, [note] = @P4 WHERE [id] = @P5",
            ])
        );
        assert_eq!(
            sql[5],
            golden::<D>([
                r#"DELETE FROM "members" WHERE "code" = $1"#,
                "DELETE FROM `members` WHERE `code` = ?",
                r#"DELETE FROM "members" WHERE "code" = ?"#,
                r#"DELETE FROM "members" WHERE "code" = :1"#,
                "DELETE FROM [members] WHERE [code] = @P1",
            ])
        );
        // Values are bound in column order, never inlined.
        assert_eq!(log.0.borrow().statements[2].1, [Param::Str("a".into()), Param::Int(1), Param::Bool(true), Param::Null]);
        assert_eq!(log.0.borrow().statements[4].1.last(), Some(&Param::Int(5)));
    });
}

fn query_statements<const D: usize>() {
    block(async {
        let (r, db, log) = repo::<D>().await;
        r.query()
            .filter_eq("name", "a")
            .filter_like("name", "a%")
            .filter_in("qty", vec!["1".into(), "2".into()])
            .order_by_desc("qty")
            .limit(10)
            .offset(20)
            .fetch_all()
            .await
            .unwrap();
        assert_eq!(
            log.last().0,
            golden::<D>([
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" WHERE "name" = $1 AND "name" LIKE $2 AND "qty" IN ($3, $4) ORDER BY "qty" DESC LIMIT 10 OFFSET 20"#,
                "SELECT `id`, `name`, `qty`, `active`, `note` FROM `items` WHERE `name` = ? AND `name` LIKE ? AND `qty` IN (?, ?) ORDER BY `qty` DESC LIMIT 10 OFFSET 20",
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" WHERE "name" = ? AND "name" LIKE ? AND "qty" IN (?, ?) ORDER BY "qty" DESC LIMIT 10 OFFSET 20"#,
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" WHERE "name" = :1 AND "name" LIKE :2 AND "qty" IN (:3, :4) ORDER BY "qty" DESC OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"#,
                "SELECT [id], [name], [qty], [active], [note] FROM [items] WHERE [name] = @P1 AND [name] LIKE @P2 AND [qty] IN (@P3, @P4) ORDER BY [qty] DESC OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY",
            ])
        );
        // A window without an order (SQL Server needs one).
        r.query().limit(5).fetch_all().await.unwrap();
        assert_eq!(
            log.last().0,
            golden::<D>([
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" LIMIT 5"#,
                "SELECT `id`, `name`, `qty`, `active`, `note` FROM `items` LIMIT 5",
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" LIMIT 5"#,
                r#"SELECT "id", "name", "qty", "active", "note" FROM "items" OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY"#,
                "SELECT [id], [name], [qty], [active], [note] FROM [items] ORDER BY (SELECT NULL) OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
            ])
        );
        let (sql, params) = db
            .select::<Item>()
            .join::<Tag>(col::<Item>("id"), "item_id")
            .group_by(col::<Item>("active"))
            .aggregate("n", Agg::Count)
            .having("n", Cmp::Gt, 1)
            .limit(3)
            .to_sql();
        assert_eq!(
            sql,
            golden::<D>([
                r#"SELECT t0."active" AS "active", COUNT(*) AS "n" FROM "items" t0 INNER JOIN "tags" t1 ON t0."id" = t1."item_id" GROUP BY t0."active" HAVING COUNT(*) > $1 LIMIT 3"#,
                "SELECT t0.`active` AS `active`, COUNT(*) AS `n` FROM `items` t0 INNER JOIN `tags` t1 ON t0.`id` = t1.`item_id` GROUP BY t0.`active` HAVING COUNT(*) > ? LIMIT 3",
                r#"SELECT t0."active" AS "active", COUNT(*) AS "n" FROM "items" t0 INNER JOIN "tags" t1 ON t0."id" = t1."item_id" GROUP BY t0."active" HAVING COUNT(*) > ? LIMIT 3"#,
                r#"SELECT t0."active" AS "active", COUNT(*) AS "n" FROM "items" t0 INNER JOIN "tags" t1 ON t0."id" = t1."item_id" GROUP BY t0."active" HAVING COUNT(*) > :1 OFFSET 0 ROWS FETCH NEXT 3 ROWS ONLY"#,
                "SELECT t0.[active] AS [active], COUNT(*) AS [n] FROM [items] t0 INNER JOIN [tags] t1 ON t0.[id] = t1.[item_id] GROUP BY t0.[active] HAVING COUNT(*) > @P1 ORDER BY (SELECT NULL) OFFSET 0 ROWS FETCH NEXT 3 ROWS ONLY",
            ])
        );
        assert_eq!(params, [Param::Int(1)]);
    });
}

fn ddl_statements<const D: usize>() {
    let d = DIALECTS[D].1;
    assert_eq!(
        d.create_table_sql(Item::table_name(), Item::columns()),
        golden::<D>([
            "CREATE TABLE IF NOT EXISTS \"items\" (\n  \"id\" BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,\n  \"name\" TEXT NOT NULL,\n  \"qty\" BIGINT NOT NULL,\n  \"active\" BOOLEAN NOT NULL,\n  \"note\" TEXT\n)",
            "CREATE TABLE IF NOT EXISTS `items` (\n  `id` BIGINT PRIMARY KEY AUTO_INCREMENT,\n  `name` LONGTEXT NOT NULL,\n  `qty` BIGINT NOT NULL,\n  `active` TINYINT(1) NOT NULL,\n  `note` LONGTEXT\n)",
            "CREATE TABLE IF NOT EXISTS \"items\" (\n  \"id\" INTEGER PRIMARY KEY AUTOINCREMENT,\n  \"name\" TEXT NOT NULL,\n  \"qty\" INTEGER NOT NULL,\n  \"active\" BOOLEAN NOT NULL,\n  \"note\" TEXT\n)",
            // '' is NULL in Oracle: text columns take no NOT NULL.
            "CREATE TABLE IF NOT EXISTS \"items\" (\n  \"id\" NUMBER(19) GENERATED BY DEFAULT ON NULL AS IDENTITY PRIMARY KEY,\n  \"name\" VARCHAR2(4000),\n  \"qty\" NUMBER(19) NOT NULL,\n  \"active\" BOOLEAN NOT NULL,\n  \"note\" VARCHAR2(4000)\n)",
            "IF OBJECT_ID(N'items', N'U') IS NULL CREATE TABLE [items] (\n  [id] BIGINT IDENTITY(1,1) PRIMARY KEY,\n  [name] NVARCHAR(MAX) NOT NULL,\n  [qty] BIGINT NOT NULL,\n  [active] BIT NOT NULL,\n  [note] NVARCHAR(MAX) NULL\n)",
        ])
    );
    assert_eq!(
        d.create_table_sql(Member::table_name(), Member::columns()),
        golden::<D>([
            "CREATE TABLE IF NOT EXISTS \"members\" (\n  \"code\" TEXT PRIMARY KEY,\n  \"email\" TEXT NOT NULL UNIQUE,\n  \"level\" INTEGER NOT NULL\n)",
            "CREATE TABLE IF NOT EXISTS `members` (\n  `code` VARCHAR(255) PRIMARY KEY,\n  `email` VARCHAR(255) NOT NULL UNIQUE,\n  `level` INT NOT NULL\n)",
            "CREATE TABLE IF NOT EXISTS \"members\" (\n  \"code\" TEXT PRIMARY KEY,\n  \"email\" TEXT NOT NULL UNIQUE,\n  \"level\" INTEGER NOT NULL\n)",
            "CREATE TABLE IF NOT EXISTS \"members\" (\n  \"code\" VARCHAR2(255) PRIMARY KEY,\n  \"email\" VARCHAR2(255) UNIQUE,\n  \"level\" NUMBER(10) NOT NULL\n)",
            "IF OBJECT_ID(N'members', N'U') IS NULL CREATE TABLE [members] (\n  [code] NVARCHAR(450) PRIMARY KEY,\n  [email] NVARCHAR(450) NOT NULL UNIQUE,\n  [level] INT NOT NULL\n)",
        ])
    );
    // A composite key is declared once, at table level.
    let cols: &'static [ColumnDef] = Box::leak(Box::new([
        ColumnDef { name: "a", field: "a", col_type: ColumnType::BigInt, nullable: false, primary_key: true, unique: false, auto: false },
        ColumnDef { name: "b", field: "b", col_type: ColumnType::BigInt, nullable: false, primary_key: true, unique: false, auto: false },
    ]));
    let ddl = d.create_table_sql("pairs", cols);
    assert!(ddl.contains(&format!("PRIMARY KEY ({}, {})", d.quote_identifier("a"), d.quote_identifier("b"))), "{}: {ddl}", DIALECTS[D].0);
    assert!(!ddl.contains("BIGINT PRIMARY KEY"), "{}: {ddl}", DIALECTS[D].0);
}

#[test]
fn golden_crud_statements_for_every_dialect() {
    each_dialect!(crud_statements);
}

#[test]
fn golden_query_statements_for_every_dialect() {
    each_dialect!(query_statements);
}

#[test]
fn golden_ddl_for_every_dialect() {
    each_dialect!(ddl_statements);
}

// ---- rules every dialect keeps ---------------------------------------------------------

fn save_flows<const D: usize>() {
    block(async {
        let (mut c, log) = conn::<D>();
        // A manual key: checked, then inserted (new) or updated (existing).
        log.0.borrow_mut().counts.extend([0, 1]);
        let m = Member { code: "A".into(), email: "a@x".into(), level: 1 };
        engine::save(&mut c, m.clone()).await.unwrap();
        engine::save(&mut c, m).await.unwrap();
        let sql = log.sql();
        assert_eq!(sql.len(), 4, "{}: {sql:?}", DIALECTS[D].0);
        assert!(sql[0].starts_with("SELECT COUNT(*) FROM") && sql[1].starts_with("INSERT INTO"), "{sql:?}");
        assert!(sql[2].starts_with("SELECT COUNT(*) FROM") && sql[3].starts_with("UPDATE"), "{sql:?}");
        assert_placeholders::<D>(&sql[1], 3);
        assert_placeholders::<D>(&sql[3], 3);
    });
}

fn batches<const D: usize>() {
    block(async {
        let rows = || (0..3).map(|i| Item { id: 0, name: format!("n{i}"), qty: i, active: true, note: None }).collect::<Vec<_>>();
        // Consecutive keys reported: one multi-row INSERT, one read-back.
        let (mut c, log) = conn::<D>();
        log.0.borrow_mut().batch_first_id = Some(40);
        log.0.borrow_mut().rows.push_back((40..43).map(|i| item_json(i, "n")).collect());
        let saved = engine::save_all(&mut c, rows()).await.unwrap();
        assert_eq!(saved.iter().map(|i| i.id).collect::<Vec<_>>(), [40, 41, 42]);
        let sql = log.sql();
        assert_eq!(sql.len(), 2, "{}: {sql:?}", DIALECTS[D].0);
        assert_placeholders::<D>(&sql[0], 12);
        assert_eq!(sql[0].matches("), (").count(), 2, "three row groups: {}", sql[0]);
        assert!(sql[1].contains(" BETWEEN "), "{}", sql[1]);
        assert_eq!(log.last().1, [Param::UInt(40), Param::UInt(42)]);
        // Keys unknown: row by row.
        let (mut c, log) = conn::<D>();
        for i in 0..3 {
            log.0.borrow_mut().rows.push_back(vec![item_json(101 + i, "n")]);
        }
        let saved = engine::save_all(&mut c, rows()).await.unwrap();
        assert_eq!(saved.iter().map(|i| i.id).collect::<Vec<_>>(), [101, 102, 103]);
        assert_eq!(log.sql().iter().filter(|s| s.starts_with("INSERT")).count(), 3);
    });
}

fn filters<const D: usize>() {
    block(async {
        let (r, _db, log) = repo::<D>().await;
        let d = DIALECTS[D].1;
        let q = |s: &str| d.quote_identifier(s);
        r.query()
            .filter_eq("name", "a")
            .filter_ne("qty", "3")
            .filter_gt("qty", "1")
            .filter_lte("qty", "9")
            .filter_between("qty", "0", "5")
            .filter_is_null("note")
            .filter_is_not_null("name")
            .fetch_count()
            .await
            .unwrap();
        let (sql, params) = log.last();
        let want = format!(
            "SELECT COUNT(*) FROM {} WHERE {} = {} AND {} <> {} AND {} > {} AND {} <= {} AND {} BETWEEN {} AND {} AND {} IS NULL AND {} IS NOT NULL",
            q("items"), q("name"), ph::<D>(1), q("qty"), ph::<D>(2), q("qty"), ph::<D>(3), q("qty"), ph::<D>(4), q("qty"), ph::<D>(5), ph::<D>(6), q("note"), q("name")
        );
        assert_eq!(sql, want, "{}", DIALECTS[D].0);
        // Numeric columns get numeric parameters, text stays text, booleans boolean.
        assert_eq!(params, [Param::Str("a".into()), Param::Int(3), Param::Int(1), Param::Int(9), Param::Int(0), Param::Int(5)]);
        r.query().filter_eq("active", "true").fetch_count().await.unwrap();
        assert_eq!(log.last().1, [Param::Bool(true)]);
        // Specs nest with parentheses, every placeholder in order.
        let spec = Spec::eq("active", "true").and(Spec::gt("qty", "1").or(Spec::lt("qty", "-1"))).not();
        r.query().filter_spec(spec).filter_eq("name", "z").fetch_all().await.unwrap();
        let (sql, params) = log.last();
        assert!(sql.contains("NOT ("), "{sql}");
        assert!(sql.contains(" OR "), "{sql}");
        assert_placeholders::<D>(&sql, 4);
        assert_eq!(params, [Param::Bool(true), Param::Int(1), Param::Int(-1), Param::Str("z".into())]);
        // Unknown fields are errors, not SQL.
        assert!(r.query().filter_eq("nope", "1").fetch_all().await.is_err());
        assert!(r.query().order_by_asc("nope").fetch_all().await.is_err());
        // Pages: a count, then the window of the page.
        log.clear();
        log.0.borrow_mut().counts.push_back(42);
        let page = r.query().order_by_asc("name").fetch_page(2, 10).await.unwrap();
        assert_eq!(page.total, 42);
        let sql = log.sql();
        assert!(sql[0].starts_with("SELECT COUNT(*)"), "{}", sql[0]);
        assert!(sql[1].ends_with(&d.window(Some(10), 20, true)), "{}: {}", DIALECTS[D].0, sql[1]);
    });
}

fn transactions<const D: usize>() {
    block(async {
        let (_r, db, log) = repo::<D>().await;
        let ok: Result<(), OrmError> = db
            .transaction(|tx| {
                Box::pin(async move {
                    tx.repository::<Item>().find_by_id_for_update(&1).await?;
                    tx.repository::<Item>().delete_by_id(&1).await?;
                    Ok(())
                })
            })
            .await;
        ok.unwrap();
        let sql = log.sql();
        assert_eq!(sql.first().map(String::as_str), Some("BEGIN"));
        assert_eq!(sql.last().map(String::as_str), Some("COMMIT"));
        let d = DIALECTS[D].1;
        let locked = &sql[1];
        assert!(
            (d.lock_for_update_suffix().is_empty() || locked.ends_with(d.lock_for_update_suffix()))
                && (d.lock_table_hint().is_empty() || locked.contains(d.lock_table_hint())),
            "{}: {locked}",
            DIALECTS[D].0
        );
        log.clear();
        let err: Result<(), OrmError> = db.transaction(|_tx| Box::pin(async move { Err(OrmError::NotFound) })).await;
        assert!(err.is_err());
        assert_eq!(log.sql(), ["BEGIN", "ROLLBACK"]);
    });
}

#[test]
fn saves_insert_or_update_by_key_kind_for_every_dialect() {
    each_dialect!(save_flows);
}

#[test]
fn batches_for_every_dialect() {
    each_dialect!(batches);
}

#[test]
fn filters_specs_and_pages_for_every_dialect() {
    each_dialect!(filters);
}

#[test]
fn transactions_bracket_their_work_for_every_dialect() {
    each_dialect!(transactions);
}

#[test]
fn entity_values_become_typed_parameters() {
    use kernway_orm_sql::{id_to_params, is_new_auto_entity, json_to_param};
    let colt = |t: ColumnType| ColumnDef { name: "c", field: "c", col_type: t, nullable: true, primary_key: false, unique: false, auto: false };
    assert_eq!(json_to_param(&serde_json::json!(3), &colt(ColumnType::Float)), Param::Float(3.0));
    assert_eq!(json_to_param(&serde_json::json!(3), &colt(ColumnType::BigInt)), Param::Int(3));
    assert_eq!(json_to_param(&serde_json::json!(u64::MAX), &colt(ColumnType::BigInt)), Param::UInt(u64::MAX));
    assert_eq!(json_to_param(&serde_json::json!([1, 2]), &colt(ColumnType::Binary)), Param::Bytes(vec![1, 2]));
    assert_eq!(json_to_param(&serde_json::json!({"a": 1}), &colt(ColumnType::Json)), Param::Str(r#"{"a":1}"#.into()));
    assert_eq!(json_to_param(&serde_json::json!(null), &colt(ColumnType::Text)), Param::Null);
    assert!(is_new_auto_entity(&Item { id: 0, name: String::new(), qty: 0, active: false, note: None }).unwrap());
    assert!(!is_new_auto_entity(&Item { id: 4, name: String::new(), qty: 0, active: false, note: None }).unwrap());
    assert!(!is_new_auto_entity(&Member { code: String::new(), email: String::new(), level: 0 }).unwrap(), "manual keys are never new");
    assert_eq!(id_to_params(&7i64).unwrap(), [Param::Int(7)]);
    assert_eq!(id_to_params(&("a".to_string(), 2i64)).unwrap(), [Param::Str("a".into()), Param::Int(2)]);
}
