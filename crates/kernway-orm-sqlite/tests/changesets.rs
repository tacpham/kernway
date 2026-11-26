//! Change capture with SQLite's session extension, as an edge → cloud sync
//! would use it: record what a transaction changed, ship the bytes, apply
//! them to another database, undo with the inverse, and handle conflicts.

use kernway_orm_core::error::OrmError;
use kernway_orm_sql::{params, Rows};
use kernway_orm_sqlite::{ChangeFormat, ChangeKind, Changeset, OnConflict, SqliteConfig, SqliteDriver};
use std::time::Duration;

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

const SCHEMA: &str = "CREATE TABLE ticket (id INTEGER PRIMARY KEY, customer TEXT NOT NULL, total INTEGER NOT NULL, status TEXT NOT NULL);
                      CREATE TABLE audit (id INTEGER PRIMARY KEY, msg TEXT);
                      INSERT INTO ticket VALUES (1, 'Lan', 3500, 'open'), (2, 'Mai', 1200, 'open');";

async fn pair() -> (SqliteDriver, SqliteDriver) {
    let edge = SqliteDriver::open_in_memory().await.unwrap();
    let cloud = SqliteDriver::open_in_memory().await.unwrap();
    edge.execute_script(SCHEMA).await.unwrap();
    cloud.execute_script(SCHEMA).await.unwrap();
    (edge, cloud)
}

/// The kinds of change in a changeset, as a sorted multiset.
async fn kinds(cs: &Changeset) -> Vec<ChangeKind> {
    let mut k: Vec<ChangeKind> = cs.operations().await.unwrap().into_iter().map(|o| o.kind).collect();
    k.sort_by_key(|k| match k {
        ChangeKind::Insert => 0,
        ChangeKind::Update => 1,
        ChangeKind::Delete => 2,
    });
    k
}

async fn tickets(db: &SqliteDriver) -> Rows {
    db.query("SELECT id, customer, total, status FROM ticket ORDER BY id", &[]).await.unwrap()
}

/// One sale on the edge: a new ticket, one paid, one voided.
async fn a_sale(edge: &SqliteDriver) -> Changeset {
    let (n, cs) = edge
        .track_changes(&["ticket"], |tx| {
            Box::pin(async move {
                tx.execute_with("INSERT INTO ticket VALUES (3, 'Hoa', 4200, 'open')", &[]).await?;
                tx.execute_with("UPDATE ticket SET status = 'paid' WHERE id = ?", params![1]).await?;
                tx.execute_with("DELETE FROM ticket WHERE id = ?", params![2]).await?;
                tx.execute_with("INSERT INTO audit (msg) VALUES ('not tracked')", &[]).await
            })
        })
        .await
        .unwrap();
    assert_eq!(n, 1, "work's own result comes back");
    cs
}

#[test]
fn a_transaction_recorded_on_one_database_replays_on_another() {
    run(async {
        let (edge, cloud) = pair().await;
        edge.execute("INSERT INTO ticket VALUES (9, 'before', 1, 'open')").await.unwrap();
        let cs = a_sale(&edge).await;
        edge.execute("INSERT INTO ticket VALUES (10, 'after', 1, 'open')").await.unwrap();

        // Exactly the transaction's changes to the tracked table (SQLite
        // groups them by table and key, not by time).
        let ops = kinds(&cs).await;
        assert_eq!(ops, [ChangeKind::Insert, ChangeKind::Update, ChangeKind::Delete]);
        assert!(cs.operations().await.unwrap().iter().all(|o| o.table == "ticket" && !o.indirect));

        // Shipped as bytes, applied on the other side.
        let shipped = Changeset::from_bytes(cs.as_bytes().to_vec(), ChangeFormat::Changeset);
        let r = cloud.apply_changeset(&shipped, OnConflict::Abort).await.unwrap();
        assert_eq!(r.conflicts, 0);
        let got = tickets(&cloud).await;
        let shape: Vec<(i64, String)> = got.rows.iter().map(|r| (r[0].as_i64().unwrap(), r[3].as_str().unwrap().into())).collect();
        assert_eq!(shape, [(1, "paid".into()), (3, "open".into())]);
        let audit = cloud.query_scalar("SELECT COUNT(*) FROM audit", &[]).await.unwrap();
        assert_eq!(audit.as_i64(), Some(0), "the untracked table was not recorded");
    });
}

#[test]
fn the_inverse_undoes_the_transaction() {
    run(async {
        let (edge, _) = pair().await;
        let before = tickets(&edge).await;
        let cs = a_sale(&edge).await;
        assert_ne!(tickets(&edge).await, before);
        let undo = cs.invert().await.unwrap();
        assert_eq!(kinds(&undo).await, [ChangeKind::Insert, ChangeKind::Update, ChangeKind::Delete], "delete ↔ insert swapped");
        edge.apply_changeset(&undo, OnConflict::Abort).await.unwrap();
        assert_eq!(tickets(&edge).await, before);
    });
}

#[test]
fn conflicts_abort_skip_or_replace() {
    run(async {
        let (edge, cloud) = pair().await;
        // Meanwhile the cloud changed ticket 1 itself.
        cloud.execute("UPDATE ticket SET status = 'refunded' WHERE id = 1").await.unwrap();
        let cs = a_sale(&edge).await;

        let e = cloud.apply_changeset(&cs, OnConflict::Abort).await.unwrap_err();
        assert!(e.to_string().contains("conflict"), "{e}");
        let untouched = tickets(&cloud).await;
        assert_eq!(untouched.rows.len(), 2, "abort applied nothing");
        assert_eq!(untouched.rows[0][3].as_str(), Some("refunded"));

        // Skip: the conflicting update is left out, the rest applies.
        let skip = SqliteDriver::open_in_memory().await.unwrap();
        skip.execute_script(SCHEMA).await.unwrap();
        skip.execute("UPDATE ticket SET status = 'refunded' WHERE id = 1").await.unwrap();
        assert_eq!(skip.apply_changeset(&cs, OnConflict::Skip).await.unwrap().conflicts, 1);
        let s = tickets(&skip).await;
        assert_eq!(s.rows.iter().map(|r| r[3].as_str().unwrap()).collect::<Vec<_>>(), ["refunded", "open"]);

        // Replace: the edge's version wins.
        assert_eq!(cloud.apply_changeset(&cs, OnConflict::Replace).await.unwrap().conflicts, 1);
        let r = tickets(&cloud).await;
        assert_eq!(r.rows.iter().map(|r| r[3].as_str().unwrap()).collect::<Vec<_>>(), ["paid", "open"]);
    });
}

#[test]
fn a_failed_or_abandoned_transaction_records_nothing_later() {
    run(async {
        let db = SqliteDriver::with_config(SqliteConfig::memory().pool_size(1)).await.unwrap();
        db.execute_script(SCHEMA).await.unwrap();
        // A failed transaction: an error, no changeset, and its changes rolled back.
        let r = db
            .track_changes(&[], |tx| {
                Box::pin(async move {
                    tx.execute_with("INSERT INTO ticket VALUES (7, 'x', 1, 'open')", &[]).await?;
                    Err::<(), _>(OrmError::Query("cashier cancelled".into()))
                })
            })
            .await;
        assert!(r.is_err());
        // An abandoned one (timed out mid-transaction) leaves a session behind
        // on the only connection…
        let abandoned = rt_core::timeout(
            Duration::from_millis(50),
            db.track_changes(&[], |tx| {
                Box::pin(async move {
                    tx.execute_with("INSERT INTO audit (msg) VALUES ('half')", &[]).await?;
                    rt_core::sleep(Duration::from_secs(10)).await;
                    Ok(())
                })
            }),
        )
        .await;
        assert!(abandoned.is_err());
        // …which never leaks into the next capture.
        let ((), cs) = db
            .track_changes(&[], |tx| Box::pin(async move { tx.execute_with("UPDATE ticket SET total = 1 WHERE id = 2", &[]).await.map(|_| ()) }))
            .await
            .unwrap();
        let ops = cs.operations().await.unwrap();
        assert_eq!(ops.len(), 1, "{ops:?}");
        assert_eq!((ops[0].table.as_str(), ops[0].kind), ("ticket", ChangeKind::Update));
    });
}

#[test]
fn patchsets_are_smaller_and_changesets_concatenate() {
    run(async {
        let (edge, cloud) = pair().await;
        let ((), patch) = edge
            .track_patch(&["ticket"], |tx| {
                Box::pin(async move { tx.execute_with("UPDATE ticket SET status = 'paid' WHERE id = 2", &[]).await.map(|_| ()) })
            })
            .await
            .unwrap();
        let ((), full) = edge
            .track_changes(&["ticket"], |tx| {
                Box::pin(async move { tx.execute_with("UPDATE ticket SET status = 'void' WHERE id = 2", &[]).await.map(|_| ()) })
            })
            .await
            .unwrap();
        assert_eq!(patch.format(), ChangeFormat::Patchset);
        assert!(patch.as_bytes().len() < full.as_bytes().len(), "a patchset carries no old values");
        assert!(matches!(patch.invert().await.unwrap_err(), OrmError::Unsupported(_)));
        assert!(patch.concat(&full).await.is_err(), "formats do not mix");
        cloud.apply_changeset(&patch, OnConflict::Abort).await.unwrap();
        assert_eq!(cloud.query_scalar("SELECT status FROM ticket WHERE id = 2", &[]).await.unwrap().as_str(), Some("paid"));

        // Two changesets merged into one: the net effect of both.
        let ((), a) = edge
            .track_changes(&[], |tx| Box::pin(async move { tx.execute_with("INSERT INTO ticket VALUES (5, 'Thu', 900, 'open')", &[]).await.map(|_| ()) }))
            .await
            .unwrap();
        let ((), b) = edge
            .track_changes(&[], |tx| Box::pin(async move { tx.execute_with("UPDATE ticket SET total = 950 WHERE id = 5", &[]).await.map(|_| ()) }))
            .await
            .unwrap();
        let both = a.concat(&b).await.unwrap();
        let ops: Vec<ChangeKind> = both.operations().await.unwrap().into_iter().map(|o| o.kind).collect();
        assert_eq!(ops, [ChangeKind::Insert], "insert then update of one row is one insert");
        let other = SqliteDriver::open_in_memory().await.unwrap();
        other.execute_script(SCHEMA).await.unwrap();
        other.apply_changeset(&both, OnConflict::Abort).await.unwrap();
        assert_eq!(other.query_scalar("SELECT total FROM ticket WHERE id = 5", &[]).await.unwrap().as_i64(), Some(950));
        // An empty capture is empty.
        let ((), none) = edge.track_changes(&[], |_tx| Box::pin(async move { Ok(()) })).await.unwrap();
        assert!(none.is_empty());
        assert!(none.operations().await.unwrap().is_empty());
    });
}
