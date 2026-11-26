//! Encodings checked against the Oracle server itself — the ground truth,
//! independent of the reference driver the wire formats were ported from.
//! Skips unless `KERNWAY_ORACLE_HOST` is set.

use crate::exec::Bind;
use crate::value::{encode_number, DateTime, Value};
use crate::{Conn, OracleConfig};

fn cfg() -> Option<OracleConfig> {
    let host = std::env::var("KERNWAY_ORACLE_HOST").ok()?;
    Some(OracleConfig::from_url(&format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1")).unwrap())
}

/// "Typ=2 Len=3: 194,2,24" -> [194, 2, 24]
fn dump_bytes(dump: &str) -> Vec<u8> {
    dump.split_once(": ")
        .map(|(_, b)| b.split(',').map(|x| x.trim().parse().unwrap()).collect())
        .unwrap_or_default()
}

const NUMBERS: &[&str] = &[
    "0", "1", "-1", "5", "-5", "10", "99", "100", "101", "-100", "123.45", "-123.45", "0.5", "-0.5",
    "0.001", "-0.001", "0.0000001", "12345678901234567890", "-12345678901234567890",
    "9223372036854775807", "-9223372036854775808", "18446744073709551615",
    "3.1415926535897932384626433832795028841", "0.3333333333333333333333333333333333333333",
    "-0.3333333333333333333333333333333333333333", "1e125", "-1e125", "9.9999999999999999999999999999999999999e125",
    "1e-130", "-1e-130", "1000000", "1000.01", "-999999.99", "25.50",
];

#[test]
fn number_encoding_matches_the_servers_own_bytes() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    rt_core::Executor::new()
        .unwrap()
        .block_on(async move {
            let mut c = Conn::connect(&cfg).await.unwrap();
            for &n in NUMBERS {
                // Oracle encodes the literal itself …
                let r = c.execute("SELECT DUMP(TO_NUMBER(:t)) FROM dual", &[Bind::from(n)]).await.unwrap();
                let server = dump_bytes(&r.rows[0][0].to_text().unwrap());
                // … and our encoder must produce the same bytes.
                assert_eq!(encode_number(n).unwrap(), server, "NUMBER {n}");
                // Our bytes, sent as a bind, compare equal to the server's value,
                // and decode back to the same number.
                let r = c
                    .execute(
                        "SELECT CASE WHEN :a = TO_NUMBER(:b) THEN 1 ELSE 0 END, TO_NUMBER(:c) FROM dual",
                        &[Bind::In(Value::Number(n.into())), Bind::from(n), Bind::from(n)],
                    )
                    .await
                    .unwrap();
                assert_eq!(r.rows[0][0].as_i64(), Some(1), "bound NUMBER {n} equals the server's");
                let back = r.rows[0][1].to_text().unwrap();
                let same = c
                    .execute("SELECT CASE WHEN TO_NUMBER(:a) = TO_NUMBER(:b) THEN 1 ELSE 0 END FROM dual", &[Bind::from(back.as_str()), Bind::from(n)])
                    .await
                    .unwrap();
                assert_eq!(same.rows[0][0].as_i64(), Some(1), "decoded {n} as {back}");
            }
        })
        .unwrap();
}

#[test]
fn dates_timestamps_and_floats_compare_equal_on_the_server() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    rt_core::Executor::new()
        .unwrap()
        .block_on(async move {
            let mut c = Conn::connect(&cfg).await.unwrap();
            let dt = |year, month, day, hour, minute, second, nanos| DateTime { year, month, day, hour, minute, second, nanos };
            for d in [
                dt(2024, 3, 15, 13, 45, 30, 0),
                dt(1, 1, 1, 0, 0, 0, 0),
                dt(9999, 12, 31, 23, 59, 59, 0),
                dt(1999, 12, 31, 23, 59, 59, 0),
                dt(2000, 2, 29, 0, 0, 0, 0),
            ] {
                let text = format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", d.year, d.month, d.day, d.hour, d.minute, d.second);
                let r = c
                    .execute(
                        "SELECT CASE WHEN :d = TO_DATE(:t, 'YYYY-MM-DD HH24:MI:SS') THEN 1 ELSE 0 END, \
                         TO_DATE(:u, 'YYYY-MM-DD HH24:MI:SS') FROM dual",
                        &[Bind::In(Value::Date(d)), Bind::from(text.as_str()), Bind::from(text.as_str())],
                    )
                    .await
                    .unwrap();
                assert_eq!(r.rows[0][0].as_i64(), Some(1), "DATE {text}");
                assert_eq!(r.rows[0][1], Value::Date(d), "DATE {text} decoded");
            }
            for d in [
                dt(2024, 3, 15, 13, 45, 30, 123_456_789),
                dt(2024, 3, 15, 13, 45, 30, 1),
                dt(2024, 3, 15, 13, 45, 30, 0),
                dt(9999, 12, 31, 23, 59, 59, 999_999_999),
            ] {
                let text = format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:09}",
                    d.year, d.month, d.day, d.hour, d.minute, d.second, d.nanos
                );
                let r = c
                    .execute(
                        "SELECT CASE WHEN :d = TO_TIMESTAMP(:t, 'YYYY-MM-DD HH24:MI:SS.FF9') THEN 1 ELSE 0 END, \
                         TO_TIMESTAMP(:u, 'YYYY-MM-DD HH24:MI:SS.FF9') FROM dual",
                        &[Bind::In(Value::Timestamp(d)), Bind::from(text.as_str()), Bind::from(text.as_str())],
                    )
                    .await
                    .unwrap();
                assert_eq!(r.rows[0][0].as_i64(), Some(1), "TIMESTAMP {text}");
                assert_eq!(r.rows[0][1], Value::Timestamp(d), "TIMESTAMP {text} decoded");
            }
            for f in [0.0, 1.5, -2.25, 1e300, -1e-300, f64::MAX, f64::MIN_POSITIVE] {
                let r = c
                    .execute(
                        "SELECT CASE WHEN :f = TO_BINARY_DOUBLE(:t) THEN 1 ELSE 0 END FROM dual",
                        &[Bind::In(Value::Float(f)), Bind::from(format!("{f:e}"))],
                    )
                    .await
                    .unwrap();
                assert_eq!(r.rows[0][0].as_i64(), Some(1), "BINARY_DOUBLE {f}");
            }
            for s in ["", "a", "Việt Nam ✓", "😀 emoji", &"x".repeat(4000)] {
                let r = c
                    .execute("SELECT LENGTH(:s), CASE WHEN :s2 = UNISTR(:u) THEN 1 ELSE 0 END FROM dual", &[
                        Bind::from(s),
                        Bind::from(s),
                        Bind::from(s.chars().map(|ch| {
                            let mut buf = [0u16; 2];
                            ch.encode_utf16(&mut buf).iter().map(|u| format!("\\{u:04X}")).collect::<String>()
                        }).collect::<String>()),
                    ])
                    .await
                    .unwrap();
                if s.is_empty() {
                    assert_eq!(r.rows[0][0], Value::Null, "Oracle stores '' as NULL");
                    continue;
                }
                // AL32UTF8 counts characters (code points), emoji included.
                assert_eq!(r.rows[0][0].as_i64(), Some(s.chars().count() as i64), "LENGTH of {s:?}");
                assert_eq!(r.rows[0][1].as_i64(), Some(1), "string {s:?} equals the server's");
            }
        })
        .unwrap();
}
