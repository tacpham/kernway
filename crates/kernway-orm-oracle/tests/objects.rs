//! Oracle object types, collections and XMLType against a real server:
//! values decoded from their pickled images with the type metadata the
//! server describes (`DBMS_PICKLER.GET_TYPE_SHAPE`), nested objects,
//! VARRAY / nested tables (of scalars and of objects, with NULLs), LOB and
//! date attributes, XMLType — and the shared API mapping them onto structs.
//! Skips unless `KERNWAY_ORACLE_HOST` is set.

use kernway_orm_oracle::OracleDriver;
use kernway_orm_sql::Value;
use serde::Deserialize;

fn url() -> Option<String> {
    let host = std::env::var("KERNWAY_ORACLE_HOST").ok()?;
    Some(format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1"))
}

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

const TYPES: &str = "
BEGIN
  FOR t IN (SELECT table_name FROM user_tables WHERE table_name = 'KWO_PEOPLE_T') LOOP
    EXECUTE IMMEDIATE 'DROP TABLE kwo_people_t';
  END LOOP;
  FOR t IN (SELECT type_name FROM user_types WHERE type_name IN ('KWO_PERSON', 'KWO_ADDR', 'KWO_TAGS', 'KWO_NUMS', 'KWO_ADDRS')
            ORDER BY DECODE(type_name, 'KWO_PERSON', 1, 'KWO_ADDRS', 2, 3)) LOOP
    EXECUTE IMMEDIATE 'DROP TYPE ' || t.type_name || ' FORCE';
  END LOOP;
END;
/
CREATE TYPE kwo_addr AS OBJECT (street VARCHAR2(100), zip NUMBER(5), since DATE)
/
CREATE TYPE kwo_tags AS VARRAY(10) OF VARCHAR2(30)
/
CREATE TYPE kwo_nums AS TABLE OF NUMBER
/
CREATE TYPE kwo_addrs AS TABLE OF kwo_addr
/
CREATE TYPE kwo_person AS OBJECT (
  name VARCHAR2(50), born TIMESTAMP(3), active BOOLEAN, home kwo_addr, tags kwo_tags,
  notes CLOB, photo BLOB, score BINARY_DOUBLE)
/
CREATE TABLE kwo_people_t (id NUMBER, p kwo_person, nums kwo_nums, past kwo_addrs, doc XMLTYPE)
  NESTED TABLE nums STORE AS kwo_people_nums NESTED TABLE past STORE AS kwo_people_past
/
INSERT INTO kwo_people_t VALUES (1,
  kwo_person('Lan', TIMESTAMP '1990-05-17 08:30:00.125', TRUE,
             kwo_addr('12 Lê Lợi', 70000, DATE '2020-01-02'),
             kwo_tags('a', NULL, 'chữ'), RPAD('n', 40000, 'n'), HEXTORAW('CAFEBABE'), 0.5),
  kwo_nums(1, 2.5, NULL, -3),
  kwo_addrs(kwo_addr('Old St', 10000, NULL)),
  XMLTYPE('<r><v id=\"1\">ok</v></r>'))
/
INSERT INTO kwo_people_t VALUES (2, kwo_person('Nam', NULL, FALSE, NULL, kwo_tags(), NULL, NULL, NULL), kwo_nums(), NULL, NULL)
/
INSERT INTO kwo_people_t VALUES (3, NULL, NULL, NULL, NULL)
/
";

#[test]
fn objects_collections_and_xmltype_columns_decode() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&url).unwrap(), 1).await.unwrap();
        db.execute_script(TYPES).await.unwrap();
        let r = db.query("SELECT id, p, nums, past, doc FROM kwo_people_t ORDER BY id", &[]).await.unwrap();
        assert_eq!(r.rows.len(), 3);

        // Row 1: every attribute kind, nested object, collections, XML.
        let Value::Object(p) = &r.rows[0][1] else { panic!("{:?}", r.rows[0][1]) };
        let get = |n: &str| p.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("NAME"), Value::Text("Lan".into()));
        assert_eq!(get("BORN"), Value::DateTime("1990-05-17 08:30:00.125000000".into()));
        assert_eq!(get("ACTIVE"), Value::Bool(true));
        assert_eq!(
            get("HOME"),
            Value::Object(vec![
                ("STREET".into(), Value::Text("12 Lê Lợi".into())),
                ("ZIP".into(), Value::Int(70000)),
                ("SINCE".into(), Value::DateTime("2020-01-02 00:00:00".into())),
            ])
        );
        assert_eq!(get("TAGS"), Value::Array(vec![Value::Text("a".into()), Value::Null, Value::Text("chữ".into())]));
        let clob_len = db
            .query_scalar("SELECT DBMS_LOB.GETLENGTH(t.p.notes) FROM kwo_people_t t WHERE id = 1", &[])
            .await
            .unwrap()
            .as_i64()
            .unwrap();
        assert!(clob_len >= 4000);
        assert_eq!(get("NOTES"), Value::Text("n".repeat(clob_len as usize)), "a CLOB attribute is read through its locator");
        assert_eq!(get("PHOTO"), Value::Bytes(vec![0xCA, 0xFE, 0xBA, 0xBE]));
        assert_eq!(get("SCORE"), Value::Float(0.5));
        assert_eq!(
            r.rows[0][2],
            Value::Array(vec![Value::Int(1), Value::Decimal("2.5".into()), Value::Null, Value::Int(-3)])
        );
        assert_eq!(
            r.rows[0][3],
            Value::Array(vec![
                Value::Object(vec![
                    ("STREET".into(), Value::Text("Old St".into())),
                    ("ZIP".into(), Value::Int(10000)),
                    ("SINCE".into(), Value::Null),
                ]),
            ])
        );
        // A NULL object element (not storable in a table: built in SQL).
        let v = db.query_scalar("SELECT kwo_addrs(kwo_addr('x', 1, NULL), NULL) FROM dual", &[]).await.unwrap();
        assert!(matches!(&v, Value::Array(a) if a.len() == 2 && a[1] == Value::Null), "{v:?}");
        assert_eq!(r.rows[0][4].as_str().map(|s| s.replace(['\n', ' '], "")), Some("<r><vid=\"1\">ok</v></r>".into()));

        // Row 2: NULL attributes, empty collections; row 3: all NULL.
        let Value::Object(p2) = &r.rows[1][1] else { panic!() };
        assert!(p2.iter().any(|(k, v)| k == "HOME" && *v == Value::Null));
        assert!(p2.iter().any(|(k, v)| k == "TAGS" && *v == Value::Array(vec![])));
        assert_eq!(r.rows[1][2], Value::Array(vec![]));
        assert!(r.rows[2][1..].iter().all(|v| *v == Value::Null));

        // Mapped onto plain structs by attribute name.
        #[derive(Debug, Deserialize, PartialEq)]
        struct Addr {
            street: String,
            zip: i32,
        }
        #[derive(Debug, Deserialize, PartialEq)]
        struct Person {
            name: String,
            active: bool,
            home: Option<Addr>,
            tags: Vec<Option<String>>,
        }
        #[derive(Debug, Deserialize)]
        struct Row {
            id: i64,
            p: Option<Person>,
            nums: Option<Vec<Option<f64>>>,
        }
        let rows: Vec<Row> = db.query("SELECT id, p, nums FROM kwo_people_t ORDER BY id", &[]).await.unwrap().to_structs().unwrap();
        assert_eq!(rows[0].id, 1);
        assert_eq!(
            rows[0].p,
            Some(Person {
                name: "Lan".into(),
                active: true,
                home: Some(Addr { street: "12 Lê Lợi".into(), zip: 70000 }),
                tags: vec![Some("a".into()), None, Some("chữ".into())],
            })
        );
        assert_eq!(rows[0].nums, Some(vec![Some(1.0), Some(2.5), None, Some(-3.0)]));
        assert!(rows[2].p.is_none());

        // An object built in SQL, and a large XMLType (stored as a CLOB).
        let v = db.query_scalar("SELECT kwo_addr('x', 1, NULL) FROM dual", &[]).await.unwrap();
        assert!(matches!(v, Value::Object(ref f) if f.len() == 3));
        let big = db
            .query_scalar("SELECT XMLELEMENT(\"r\", XMLAGG(XMLELEMENT(\"v\", LEVEL))) FROM dual CONNECT BY LEVEL <= 5000", &[])
            .await
            .unwrap();
        let text = big.as_str().unwrap();
        assert!(text.len() > 50_000, "{}", text.len());
        assert_eq!(text.matches("<v>").count(), 5000);
        assert!(text.contains("<v>4999</v>"));
        assert_eq!(db.query_scalar("SELECT 9 FROM dual", &[]).await.unwrap(), Value::Int(9));
    });
}

#[test]
fn object_collection_and_record_arguments_through_call() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        use kernway_orm_sql::{CallArg, OutKind, Param};
        let db = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&url).unwrap(), 1).await.unwrap();
        db.execute_script(TYPES).await.unwrap();
        db.execute_script(
            "CREATE OR REPLACE PACKAGE kwo_api2 AS
  TYPE rec IS RECORD (id PLS_INTEGER, name VARCHAR2(30), ok BOOLEAN, amt NUMBER(10,2), born DATE);
  TYPE idx_tab IS TABLE OF VARCHAR2(20) INDEX BY PLS_INTEGER;
  FUNCTION make_addr(p_street VARCHAR2) RETURN kwo_addr;
  PROCEDURE echo_tags(p_in IN kwo_tags, p_n OUT NUMBER, p_back OUT kwo_tags);
  FUNCTION sum_nums(p IN kwo_nums) RETURN NUMBER;
  PROCEDURE grow_addrs(p IN OUT kwo_addrs);
  FUNCTION echo_rec(p IN rec) RETURN rec;
  FUNCTION join_idx(p IN idx_tab) RETURN VARCHAR2;
  FUNCTION notes_len(p IN kwo_person) RETURN NUMBER;
END;
/
CREATE OR REPLACE PACKAGE BODY kwo_api2 AS
  FUNCTION make_addr(p_street VARCHAR2) RETURN kwo_addr IS BEGIN RETURN kwo_addr(p_street, 12345, DATE '2026-11-26'); END;
  PROCEDURE echo_tags(p_in IN kwo_tags, p_n OUT NUMBER, p_back OUT kwo_tags) IS
  BEGIN p_n := p_in.COUNT; p_back := p_in; p_back.EXTEND; p_back(p_back.COUNT) := 'added'; END;
  FUNCTION sum_nums(p IN kwo_nums) RETURN NUMBER IS s NUMBER := 0;
  BEGIN FOR i IN 1 .. p.COUNT LOOP s := s + NVL(p(i), 0); END LOOP; RETURN s; END;
  PROCEDURE grow_addrs(p IN OUT kwo_addrs) IS
  BEGIN p.EXTEND; p(p.COUNT) := kwo_addr('new ' || p.COUNT, p.COUNT, NULL); END;
  FUNCTION echo_rec(p IN rec) RETURN rec IS r rec := p;
  BEGIN r.id := p.id * 2; r.name := UPPER(p.name); r.ok := NOT p.ok; r.amt := p.amt + 0.25; r.born := p.born + 1; RETURN r; END;
  FUNCTION join_idx(p IN idx_tab) RETURN VARCHAR2 IS k PLS_INTEGER := p.FIRST; s VARCHAR2(400);
  BEGIN WHILE k IS NOT NULL LOOP s := s || k || '=' || p(k) || ';'; k := p.NEXT(k); END LOOP; RETURN s; END;
  FUNCTION notes_len(p IN kwo_person) RETURN NUMBER IS
  BEGIN RETURN DBMS_LOB.GETLENGTH(p.notes) + NVL(DBMS_LOB.GETLENGTH(p.photo), 0) * 1000000 + p.tags.COUNT * 1000000000; END;
END;
/",
        )
        .await
        .unwrap();
        let s = |x: &str| Param::Str(x.into());
        let tv = |x: &str| Value::Text(x.into());

        // OUT object (function result), type from the declaration.
        let r = db.call_function("kwo_api2.make_addr", OutKind::Object, &["Hải Phòng".into()]).await.unwrap();
        assert_eq!(
            r.value,
            Value::Object(vec![
                ("STREET".into(), tv("Hải Phòng")),
                ("ZIP".into(), Value::Int(12345)),
                ("SINCE".into(), Value::DateTime("2026-11-26 00:00:00".into())),
            ])
        );

        // IN VARRAY (with a NULL), OUT number and OUT VARRAY.
        let tags = Param::Array(vec![s("x"), Param::Null, s("chữ")]);
        let r = db.call("kwo_api2.echo_tags", &[tags.into(), OutKind::Number.into(), OutKind::Object.into()]).await.unwrap();
        assert_eq!(r.out[0], Value::Int(3));
        assert_eq!(r.out[1], Value::Array(vec![tv("x"), Value::Null, tv("chữ"), tv("added")]));
        // A VARRAY(10) refuses 11 elements before anything is sent.
        let too_many = Param::Array((0..11).map(|i| s(&i.to_string())).collect());
        let e = db.call("kwo_api2.echo_tags", &[too_many.into(), OutKind::Number.into(), OutKind::Object.into()]).await.unwrap_err();
        assert!(e.to_string().contains("at most 10"), "{e}");

        // IN nested table of numbers (large), named argument.
        let nums = Param::Array((1..=3000).map(|i| Param::Float(f64::from(i) / 4.0)).collect());
        let r = db.call_function("kwo_api2.sum_nums", OutKind::Number, &[CallArg::named("p", nums)]).await.unwrap();
        assert_eq!(r.value.as_f64(), Some((1..=3000).map(|i| f64::from(i) / 4.0).sum::<f64>()));

        // IN OUT nested table of objects.
        let addrs = Param::Array(vec![Param::Object(vec![("street".into(), s("a")), ("zip".into(), Param::Int(1))])]);
        let r = db.call("kwo_api2.grow_addrs", &[CallArg::InOut(addrs)]).await.unwrap();
        let Value::Array(out) = &r.out[0] else { panic!("{:?}", r.out[0]) };
        assert_eq!(out.len(), 2);
        assert_eq!(out[1], Value::Object(vec![("STREET".into(), tv("new 2")), ("ZIP".into(), Value::Int(2)), ("SINCE".into(), Value::Null)]));

        // PL/SQL record in and out: PLS_INTEGER, BOOLEAN, NUMBER(10,2), DATE.
        let rec = Param::Object(vec![
            ("id".into(), Param::Int(-21)),
            ("name".into(), s("lan")),
            ("ok".into(), Param::Bool(true)),
            ("amt".into(), Param::Float(10.5)),
            ("born".into(), s("2000-02-28")),
        ]);
        let r = db.call_function("kwo_api2.echo_rec", OutKind::Object, &[rec.into()]).await.unwrap();
        assert_eq!(
            r.value,
            Value::Object(vec![
                ("ID".into(), Value::Int(-42)),
                ("NAME".into(), tv("LAN")),
                ("OK".into(), Value::Bool(false)),
                ("AMT".into(), Value::Decimal("10.75".into())),
                ("BORN".into(), Value::DateTime("2000-02-29 00:00:00".into())),
            ])
        );

        // Associative array (index-by): keys kept, sorted by the server.
        let idx = Param::Object(vec![("10".into(), s("ten")), ("-3".into(), s("minus three")), ("2".into(), s("two"))]);
        let r = db.call_function("kwo_api2.join_idx", OutKind::Text, &[idx.into()]).await.unwrap();
        assert_eq!(r.value, tv("-3=minus three;2=two;10=ten;"));

        // An object with LOB attributes (> 32 KB) and a nested VARRAY.
        let person = Param::Object(vec![
            ("name".into(), s("big")),
            ("notes".into(), s(&"n".repeat(50_000))),
            ("photo".into(), Param::Bytes(vec![7; 70])),
            ("tags".into(), Param::Array(vec![s("a"), s("b")])),
        ]);
        let r = db.call_function("kwo_api2.notes_len", OutKind::Number, &[person.into()]).await.unwrap();
        assert_eq!(r.value, Value::Int(2_000_000_000 + 70_000_000 + 50_000));
        // The temporary LOBs made for those attributes were freed.
        let left = db
            .query_scalar(
                "SELECT NVL(SUM(cache_lobs + nocache_lobs + abstract_lobs), 0) FROM v$temporary_lobs WHERE sid = SYS_CONTEXT('USERENV', 'SID')",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(left, Value::Int(0), "no temporary LOB left behind");

        // A field the type does not have is an error, not silently dropped.
        let bad = Param::Object(vec![("nope".into(), Param::Int(1))]);
        assert!(db.call_function("kwo_api2.echo_rec", OutKind::Object, &[bad.into()]).await.is_err());
        assert_eq!(db.query_scalar("SELECT 3 FROM dual", &[]).await.unwrap(), Value::Int(3));
    });
}

/// An object column next to LOB columns: the LOBs make the driver define
/// the columns again, and the object must still be read in its own format
/// (it was read as plain bytes, which threw the response out of step —
/// "unexpected TTC message type 36" — e.g. on `SELECT *` of an AQ queue
/// table: USER_DATA BLOB next to USER_PROP SYS.ANYDATA).
#[test]
fn objects_next_to_lob_columns_decode() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&url).unwrap(), 1).await.unwrap();
        db.execute_script(
            "BEGIN
               FOR t IN (SELECT table_name FROM user_tables WHERE table_name = 'KWO_LOBMIX_T') LOOP EXECUTE IMMEDIATE 'DROP TABLE kwo_lobmix_t'; END LOOP;
               FOR t IN (SELECT type_name FROM user_types WHERE type_name = 'KWO_LOBMIX_PT') LOOP EXECUTE IMMEDIATE 'DROP TYPE kwo_lobmix_pt FORCE'; END LOOP;
             END;
             /
             CREATE TYPE kwo_lobmix_pt AS OBJECT (x NUMBER, label VARCHAR2(20))
             /
             CREATE TABLE kwo_lobmix_t (id NUMBER, data BLOB, p kwo_lobmix_pt, note CLOB, tail VARCHAR2(10))
             /
             INSERT INTO kwo_lobmix_t VALUES (1, HEXTORAW('DEADBEEF'), kwo_lobmix_pt(7, 'seven'), 'first', 'a')
             /
             INSERT INTO kwo_lobmix_t VALUES (2, NULL, NULL, NULL, 'b')
             /
             INSERT INTO kwo_lobmix_t VALUES (3, HEXTORAW('00'), kwo_lobmix_pt(NULL, 'three'), RPAD('x', 5000, 'x'), 'c')
             /",
        )
        .await
        .unwrap();
        for sql in ["SELECT * FROM kwo_lobmix_t ORDER BY id", "SELECT p, data, tail FROM kwo_lobmix_t ORDER BY id"] {
            let r = db.query(sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert_eq!(r.rows.len(), 3, "{sql}");
            let col = |row: usize, name: &str| r.get(row, name).cloned().unwrap_or_else(|| panic!("{name}"));
            assert_eq!(col(0, "P"), Value::Object(vec![("X".into(), Value::Int(7)), ("LABEL".into(), Value::Text("seven".into()))]), "{sql}");
            assert_eq!(col(1, "P"), Value::Null);
            assert_eq!(col(2, "P"), Value::Object(vec![("X".into(), Value::Null), ("LABEL".into(), Value::Text("three".into()))]));
            assert_eq!(col(0, "DATA"), Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
            assert_eq!(col(2, "TAIL"), Value::Text("c".into()), "the columns after the object stay in step");
        }
        // Through a cursor too, in small batches.
        let mut cur = db.cursor("SELECT * FROM kwo_lobmix_t ORDER BY id", &[]).await.unwrap();
        let mut n = 0;
        while let Some(b) = cur.next_batch(2).await.unwrap() {
            n += b.rows.len();
        }
        assert_eq!(n, 3);
    });
}
