//! Two-phase commit against a real Oracle: a branch begun, ended,
//! prepared and committed; committed from another session after the first
//! is gone (recovery); rolled back; one-phase commit; a read-only branch.
//! Data visibility is checked from an independent session.
//! Skips unless `KERNWAY_ORACLE_HOST` is set.

use kernway_orm_oracle::{Bind, Conn, OracleConfig, TpcBegin, Value, Xid};

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn xid(n: u32) -> Xid {
    Xid { format_id: 7, global_id: format!("kernway-gtx-{n}-{}", std::process::id()).into_bytes(), branch: b"b1".to_vec() }
}

async fn count(c: &mut Conn, tag: &str) -> i64 {
    let r = c.execute("SELECT COUNT(*) FROM kwo_tpc WHERE tag = :1", &[Bind::In(Value::Str(tag.into()))]).await.unwrap();
    r.rows[0][0].as_i64().unwrap()
}

#[test]
fn two_phase_commit_prepare_commit_recover_and_rollback() {
    let Ok(host) = std::env::var("KERNWAY_ORACLE_HOST") else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let cfg = OracleConfig::from_url(&format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1")).unwrap();
        let mut watch = Conn::connect(&cfg).await.unwrap();
        let _ = watch.execute("DROP TABLE kwo_tpc", &[]).await;
        watch.execute("CREATE TABLE kwo_tpc (tag VARCHAR2(20))", &[]).await.unwrap();
        let insert = |tag: &str| ("INSERT INTO kwo_tpc VALUES (:1)", vec![Bind::In(Value::Str(tag.into()))]);

        // begin → work → end → prepare → commit (two phases).
        let mut a = Conn::connect(&cfg).await.unwrap();
        let x = xid(1);
        a.tpc_begin(&x, TpcBegin::New, 60).await.unwrap();
        let (sql, b) = insert("two-phase");
        a.execute(sql, &b).await.unwrap();
        a.tpc_end(&x, false).await.unwrap();
        assert_eq!(count(&mut watch, "two-phase").await, 0, "not visible before commit");
        assert!(a.tpc_prepare(&x).await.unwrap(), "changed data: needs a commit");
        assert_eq!(count(&mut watch, "two-phase").await, 0, "prepared is still not committed");
        a.tpc_commit(&x, false).await.unwrap();
        assert_eq!(count(&mut watch, "two-phase").await, 1);

        // Recovery: prepared in one session, which then goes away; another
        // session commits it by its XID.
        let x = xid(2);
        {
            let mut b = Conn::connect(&cfg).await.unwrap();
            b.tpc_begin(&x, TpcBegin::New, 60).await.unwrap();
            let (sql, binds) = insert("recovered");
            b.execute(sql, &binds).await.unwrap();
            b.tpc_end(&x, false).await.unwrap();
            assert!(b.tpc_prepare(&x).await.unwrap());
            b.close().await.unwrap();
        }
        assert_eq!(count(&mut watch, "recovered").await, 0);
        let mut c = Conn::connect(&cfg).await.unwrap();
        c.tpc_commit(&x, false).await.unwrap();
        assert_eq!(count(&mut watch, "recovered").await, 1, "committed from another session");

        // Rollback of a prepared branch.
        let x = xid(3);
        a.tpc_begin(&x, TpcBegin::New, 60).await.unwrap();
        let (sql, binds) = insert("rolled-back");
        a.execute(sql, &binds).await.unwrap();
        a.tpc_end(&x, false).await.unwrap();
        a.tpc_prepare(&x).await.unwrap();
        a.tpc_rollback(&x).await.unwrap();
        assert_eq!(count(&mut watch, "rolled-back").await, 0);

        // One-phase commit (no prepare), and a read-only branch.
        let x = xid(4);
        a.tpc_begin(&x, TpcBegin::New, 60).await.unwrap();
        let (sql, binds) = insert("one-phase");
        a.execute(sql, &binds).await.unwrap();
        a.tpc_end(&x, false).await.unwrap();
        a.tpc_commit(&x, true).await.unwrap();
        assert_eq!(count(&mut watch, "one-phase").await, 1);
        let x = xid(5);
        a.tpc_begin(&x, TpcBegin::New, 60).await.unwrap();
        a.execute("SELECT COUNT(*) FROM kwo_tpc", &[]).await.unwrap();
        a.tpc_end(&x, false).await.unwrap();
        assert!(!a.tpc_prepare(&x).await.unwrap(), "a read-only branch has nothing to commit");

        // Suspend and resume a branch.
        let x = xid(6);
        a.tpc_begin(&x, TpcBegin::New, 60).await.unwrap();
        let (sql, binds) = insert("resumed");
        a.execute(sql, &binds).await.unwrap();
        a.tpc_end(&x, true).await.unwrap();
        a.tpc_begin(&x, TpcBegin::Resume, 60).await.unwrap();
        let (sql, binds) = insert("resumed");
        a.execute(sql, &binds).await.unwrap();
        a.tpc_end(&x, false).await.unwrap();
        a.tpc_commit(&x, true).await.unwrap();
        assert_eq!(count(&mut watch, "resumed").await, 2);
    });
}
