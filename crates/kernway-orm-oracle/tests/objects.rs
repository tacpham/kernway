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
