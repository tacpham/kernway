//! Advanced Queuing against a real Oracle: RAW, JSON and object-type
//! queues; enqueue / dequeue with correlation, priority, delay; browse,
//! wait with timeout on an empty queue, visibility with the transaction.
//! Skips unless `KERNWAY_ORACLE_HOST` is set.

use kernway_orm_oracle::{AqDequeue, AqMessage, AqPayload, Conn, OracleConfig, Value};

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

const SETUP: &str = "
DECLARE
  PROCEDURE q(name VARCHAR2, payload VARCHAR2) IS
  BEGIN
    BEGIN DBMS_AQADM.STOP_QUEUE(name); EXCEPTION WHEN OTHERS THEN NULL; END;
    BEGIN DBMS_AQADM.DROP_QUEUE(name); EXCEPTION WHEN OTHERS THEN NULL; END;
    BEGIN DBMS_AQADM.DROP_QUEUE_TABLE(name || '_t', TRUE); EXCEPTION WHEN OTHERS THEN NULL; END;
    DBMS_AQADM.CREATE_QUEUE_TABLE(queue_table => name || '_t', queue_payload_type => payload);
    DBMS_AQADM.CREATE_QUEUE(queue_name => name, queue_table => name || '_t');
    DBMS_AQADM.START_QUEUE(name);
  END;
BEGIN
  q('kwo_q_raw', 'RAW');
  q('kwo_q_json', 'JSON');
  q('kwo_q_obj', 'KWO_ADDR');
END;";

#[test]
fn advanced_queuing_raw_json_and_object_queues() {
    let Ok(host) = std::env::var("KERNWAY_ORACLE_HOST") else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let pw = std::env::var("KERNWAY_ORACLE_SYSTEM_PASSWORD").unwrap_or_else(|_| "oracle_pw".into());
        let mut sys = Conn::connect(&OracleConfig::from_url(&format!("oracle://system:{pw}@{host}:1521/FREEPDB1")).unwrap()).await.unwrap();
        sys.execute("GRANT AQ_ADMINISTRATOR_ROLE TO kernway", &[]).await.unwrap();
        sys.execute("GRANT EXECUTE ON DBMS_AQ TO kernway", &[]).await.unwrap();
        sys.execute("GRANT EXECUTE ON DBMS_AQADM TO kernway", &[]).await.unwrap();
        let cfg = OracleConfig::from_url(&format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1")).unwrap();
        let mut c = Conn::connect(&cfg).await.unwrap();
        // The object type the object queue carries.
        let _ = c.execute("CREATE TYPE kwo_addr AS OBJECT (street VARCHAR2(100), zip NUMBER(5), since DATE)", &[]).await;
        c.execute(SETUP, &[]).await.unwrap();
        let now = AqDequeue { immediate: true, ..Default::default() };

        // RAW: two messages, one picked by correlation, then the other.
        let mut m = AqMessage::new(Value::Raw(vec![1, 2, 3]));
        m.correlation = Some("first".into());
        let id1 = c.aq_enqueue("kwo_q_raw", &AqPayload::Raw, &m, true).await.unwrap();
        assert_eq!(id1.len(), 16);
        let mut m2 = AqMessage::new(Value::Raw(vec![0xFF; 2000]));
        m2.correlation = Some("second".into());
        c.aq_enqueue("kwo_q_raw", &AqPayload::Raw, &m2, true).await.unwrap();
        // The queue table itself reads with every column — USER_DATA (BLOB)
        // next to USER_PROP (SYS.ANYDATA) — also right after a failed statement.
        assert!(c.execute("SELECT * FROM kwo_no_such_table", &[]).await.is_err());
        let mut first = Vec::new();
        let (cols, _) = c.open_cursor("SELECT * FROM kwo_q_raw_t ORDER BY enq_time", &[], &mut first).await.unwrap();
        assert!(cols.iter().any(|c| c.name == "USER_PROP") && cols.iter().any(|c| c.name == "USER_DATA"));
        assert_eq!(first.len(), 2, "both messages are rows of the queue table");
        let data = cols.iter().position(|c| c.name == "USER_DATA").unwrap();
        assert_eq!(first[0][data], Value::Raw(vec![1, 2, 3]));
        let got = c
            .aq_dequeue("kwo_q_raw", &AqPayload::Raw, &AqDequeue { correlation: Some("second".into()), ..now.clone() })
            .await
            .unwrap()
            .unwrap();
        assert_eq!((got.payload, got.correlation.as_deref()), (Value::Raw(vec![0xFF; 2000]), Some("second")));
        // Browse leaves it in the queue; a real dequeue removes it.
        let seen = c.aq_dequeue("kwo_q_raw", &AqPayload::Raw, &AqDequeue { browse: true, ..now.clone() }).await.unwrap().unwrap();
        assert_eq!(seen.msg_id, id1);
        let got = c.aq_dequeue("kwo_q_raw", &AqPayload::Raw, &now).await.unwrap().unwrap();
        assert_eq!((got.payload, got.msg_id), (Value::Raw(vec![1, 2, 3]), id1));
        // Empty queue: no wait, and a 1-second wait, both None.
        assert!(c.aq_dequeue("kwo_q_raw", &AqPayload::Raw, &now).await.unwrap().is_none());
        let t = std::time::Instant::now();
        assert!(c.aq_dequeue("kwo_q_raw", &AqPayload::Raw, &AqDequeue { wait: Some(1), ..now.clone() }).await.unwrap().is_none());
        assert!(t.elapsed() >= std::time::Duration::from_millis(900), "waited on the server");

        // JSON payloads.
        let doc = r#"{"order":42,"items":["a","b"],"note":"chữ"}"#;
        c.aq_enqueue("kwo_q_json", &AqPayload::Json, &AqMessage::new(Value::Str(doc.into())), true).await.unwrap();
        let got = c.aq_dequeue("kwo_q_json", &AqPayload::Json, &now).await.unwrap().unwrap();
        assert_eq!(got.payload, Value::Str(doc.into()));

        // Object payloads, and visibility with the transaction.
        let addr = Value::Object {
            type_name: String::new(),
            fields: vec![("STREET".into(), Value::Str("1 Queue St".into())), ("ZIP".into(), Value::Number("123".into()))],
        };
        c.aq_enqueue("kwo_q_obj", &AqPayload::Object("KERNWAY.KWO_ADDR".into()), &AqMessage::new(addr), false).await.unwrap();
        let mut other = Conn::connect(&cfg).await.unwrap();
        let obj = AqPayload::Object("KERNWAY.KWO_ADDR".into());
        assert!(other.aq_dequeue("kwo_q_obj", &obj, &now).await.unwrap().is_none(), "not visible before commit");
        c.commit().await.unwrap();
        let got = other.aq_dequeue("kwo_q_obj", &obj, &now).await.unwrap().unwrap();
        let Value::Object { fields, .. } = got.payload else { panic!() };
        assert_eq!(fields[0], ("STREET".into(), Value::Str("1 Queue St".into())));
        assert_eq!(fields[1], ("ZIP".into(), Value::Number("123".into())));

        // A delayed message is not dequeuable yet.
        let mut later = AqMessage::new(Value::Raw(vec![9]));
        later.delay = 60;
        c.aq_enqueue("kwo_q_raw", &AqPayload::Raw, &later, true).await.unwrap();
        assert!(c.aq_dequeue("kwo_q_raw", &AqPayload::Raw, &now).await.unwrap().is_none(), "delayed 60 s");
    });
}
