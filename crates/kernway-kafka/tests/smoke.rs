//! Low-level smoke tests: the handshake and a produce / fetch round trip.

use kernway_kafka::conn::BrokerConn;
use kernway_kafka::protocol::{Api, R, W};
use kernway_kafka::Options;

#[test]
fn handshake_and_metadata() {
    let Ok(url) = std::env::var("KERNWAY_KAFKA_URL") else {
        eprintln!("skip: KERNWAY_KAFKA_URL not set");
        return;
    };
    rt_core::Executor::new().unwrap().block_on(async move {
        let o = Options::parse(&url).unwrap();
        let (h, p) = o.bootstrap[0].clone();
        let c = BrokerConn::connect(&o, &h, p).await.unwrap();
        let mut w = W::new(true);
        w.uvarint(0).bool(false).bool(false).tags(); // topics: null → all
        let b = c.call(Api::Metadata, &w.b).await.unwrap();
        let mut r = R::new(&b, true);
        r.i32().unwrap(); // throttle
        let n = r.array_len().unwrap();
        assert!(n >= 1, "at least one broker");
        let (id, host, port) = (r.i32().unwrap(), r.string().unwrap(), r.i32().unwrap());
        eprintln!("broker {id} at {host}:{port}");
        assert_eq!(port, 9092);
    }).unwrap();
}

#[test]
fn produce_fetch_offsets() {
    let Ok(url) = std::env::var("KERNWAY_KAFKA_URL") else { return };
    rt_core::Executor::new().unwrap().block_on(async move {
        use kernway_kafka::client::Client;
        use kernway_kafka::records::Record;
        let c = Client::connect(Options::parse(&url).unwrap()).await.unwrap();
        let t = format!("kw-smoke-{}", std::process::id());
        assert!(c.create_topic(&t, 3, 1).await.unwrap());
        assert!(!c.create_topic(&t, 3, 1).await.unwrap(), "exists");
        let p = c.choose_partition(&t, Some(b"key")).await.unwrap();
        for i in 0..5 {
            let off = c.produce(&t, p, Record { timestamp: 1, key: Some(b"key".to_vec()), value: Some(format!("v{i}").into_bytes()), headers: vec![("h".into(), Some(b"x".to_vec()))], ..Record::default() }).await.unwrap();
            assert_eq!(off, i);
        }
        let f = c.fetch(&t, p, 2, std::time::Duration::from_millis(500), 1 << 20).await.unwrap();
        assert_eq!(f.high_watermark, 5);
        let vals: Vec<_> = f.records.iter().map(|r| String::from_utf8(r.value.clone().unwrap()).unwrap()).collect();
        assert_eq!(vals, ["v2", "v3", "v4"]);
        assert_eq!(f.records[0].headers[0].0, "h");
        assert_eq!(c.list_offsets(&t, -1).await.unwrap()[&p], 5);
        assert_eq!(c.list_offsets(&t, -2).await.unwrap()[&p], 0);
        assert!(c.delete_topic(&t).await.unwrap());
    }).unwrap();
}

