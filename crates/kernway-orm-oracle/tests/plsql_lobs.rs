//! CLOB / BLOB through PL/SQL against a real Oracle: OUT arguments and
//! function results of any size (read through their locators), IN values past
//! the 32 KB PL/SQL bind limit (sent as temporary LOBs), IN OUT, NULL — each
//! checked against what the server itself computes (lengths, substrings,
//! a stored copy read back by SQL).
//! Skips unless `KERNWAY_ORACLE_HOST` is set.

use kernway_orm_oracle::{Bind, Conn, OracleConfig, OutType, Value};

fn cfg() -> Option<OracleConfig> {
    let host = std::env::var("KERNWAY_ORACLE_HOST").ok()?;
    Some(OracleConfig::from_url(&format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1")).unwrap())
}

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

/// Text with 1-, 2-, 3- and 4-byte UTF-8 characters (UTF-16 surrogates too).
/// 21 characters, 22 UTF-16 units: CLOB lengths and offsets count units.
const PIECE: &str = "Việt Nam ✓ 0123456 😀 ";

fn num(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("not a number: {v:?}"))
}

#[test]
fn out_clob_and_blob_of_any_size_come_back_whole() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let mut c = Conn::connect(&cfg).await.unwrap();
        // ~110k characters built by the server.
        let r = c
            .execute(
                "declare c clob; s varchar2(100) := :p; begin
                   dbms_lob.createtemporary(c, true);
                   for i in 1 .. 5000 loop dbms_lob.append(c, to_clob(s)); end loop;
                   :c := c; :n := dbms_lob.getlength(c);
                 end;",
                &[Bind::In(Value::Str(PIECE.into())), Bind::Out(OutType::Clob), Bind::Out(OutType::Number)],
            )
            .await
            .unwrap();
        let expect = PIECE.repeat(5000);
        assert_eq!(r.out[1], Value::Str(expect.clone()));
        assert_eq!(num(&r.out[2]) as usize, expect.encode_utf16().count(), "server length (UTF-16 units)");

        let r = c
            .execute(
                "declare b blob; begin
                   dbms_lob.createtemporary(b, true);
                   for i in 1 .. 3000 loop dbms_lob.writeappend(b, 16, hextoraw('000102030405060708090A0B0C0D0EFF')); end loop;
                   :b := b; :n := dbms_lob.getlength(b);
                 end;",
                &[Bind::Out(OutType::Blob), Bind::Out(OutType::Number)],
            )
            .await
            .unwrap();
        let unit: Vec<u8> = (0u8..15).chain([0xFF]).collect();
        assert_eq!(r.out[0], Value::Raw(unit.repeat(3000)));
        assert_eq!(num(&r.out[1]), 48_000);

        // NULL and empty.
        let r = c
            .execute("begin :a := null; :b := empty_clob(); :c := empty_blob(); end;", &[
                Bind::Out(OutType::Clob),
                Bind::Out(OutType::Clob),
                Bind::Out(OutType::Blob),
            ])
            .await
            .unwrap();
        assert_eq!(r.out[0], Value::Null);
        assert_eq!(r.out[1], Value::Str(String::new()));
        assert_eq!(r.out[2], Value::Raw(Vec::new()));

        // A function returning a CLOB, called from PL/SQL.
        let _ = c.execute("drop function kw_lob_fn", &[]).await;
        c.execute(
            "create function kw_lob_fn(n number) return clob is c clob; begin
               dbms_lob.createtemporary(c, true);
               for i in 1 .. n loop dbms_lob.writeappend(c, 10, 'abcdefghij'); end loop;
               return c; end;",
            &[],
        )
        .await
        .unwrap();
        let r = c.execute("begin :r := kw_lob_fn(:n); end;", &[Bind::Out(OutType::Clob), Bind::In(Value::Number("7000".into()))]).await.unwrap();
        assert_eq!(r.out[0], Value::Str("abcdefghij".repeat(7000)));
        // The session's temporary LOBs were freed after reading.
        let r = c
            .execute(
                "select nvl(sum(cache_lobs + nocache_lobs + abstract_lobs), 0) from v$temporary_lobs where sid = sys_context('userenv', 'sid')",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(num(&r.rows[0][0]), 0, "no temporary LOB left behind");
    });
}

#[test]
fn in_values_past_32k_travel_as_temporary_lobs() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let mut c = Conn::connect(&cfg).await.unwrap();
        let big = PIECE.repeat(10_000); // ~260 KB of UTF-8
        let bytes: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 256) as u8).collect();
        let r = c
            .execute(
                "begin :n := dbms_lob.getlength(:c); :s := dbms_lob.substr(:c, 22, 22 * 4000 + 1);
                       :bn := dbms_lob.getlength(:b); :bs := dbms_lob.substr(:b, 4, 199997); end;",
                &[
                    Bind::Out(OutType::Number),
                    Bind::In(Value::Str(big.clone())),
                    Bind::Out(OutType::Str(200)),
                    Bind::Out(OutType::Number),
                    Bind::In(Value::Raw(bytes.clone())),
                    Bind::Out(OutType::Raw(10)),
                ],
            )
            .await
            .unwrap();
        assert_eq!(num(&r.out[0]) as usize, big.encode_utf16().count());
        assert_eq!(r.out[2], Value::Str(PIECE.into()), "the server sees the exact characters");
        assert_eq!(num(&r.out[3]), 200_000);
        assert_eq!(r.out[5], Value::Raw(bytes[199_996..].to_vec()));

        // Stored by PL/SQL, read back by SQL: byte for byte.
        let _ = c.execute("drop table kw_lob_t", &[]).await;
        c.execute("create table kw_lob_t (id number, c clob, b blob)", &[]).await.unwrap();
        c.execute(
            "begin insert into kw_lob_t values (1, :c, :b); commit; end;",
            &[Bind::In(Value::Str(big.clone())), Bind::In(Value::Raw(bytes.clone()))],
        )
        .await
        .unwrap();
        let back = c.execute("select c, b from kw_lob_t where id = 1", &[]).await.unwrap();
        assert_eq!(back.rows[0][0], Value::Str(big.clone()));
        assert_eq!(back.rows[0][1], Value::Raw(bytes.clone()));
    });
}

#[test]
fn in_out_clob_and_blob() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let mut c = Conn::connect(&cfg).await.unwrap();
        let r = c
            .execute(
                "begin dbms_lob.append(:c, ' + tail ✓'); :b := utl_raw.concat(dbms_lob.substr(:b, 3, 1), hextoraw('FF')); end;",
                &[Bind::InOut(Value::Str("head".into()), OutType::Clob), Bind::InOut(Value::Raw(vec![1, 2, 3, 4]), OutType::Blob)],
            )
            .await
            .unwrap();
        assert_eq!(r.out[0], Value::Str("head + tail ✓".into()));
        assert_eq!(r.out[1], Value::Raw(vec![1, 2, 3, 0xFF]));
        // Large in, larger out.
        let big = "x".repeat(70_000);
        let r = c
            .execute("begin dbms_lob.append(:c, :c); end;", &[Bind::InOut(Value::Str(big.clone()), OutType::Clob)])
            .await
            .unwrap();
        assert_eq!(r.out[0], Value::Str(big.repeat(2)));
        // NULL in.
        let r = c.execute("begin :c := nvl(:c, empty_clob()); end;", &[Bind::InOut(Value::Null, OutType::Clob)]).await.unwrap();
        assert_eq!(r.out[0], Value::Str(String::new()));
    });
}
