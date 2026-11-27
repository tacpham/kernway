//! The messaging conformance suite against a live Kafka (`KERNWAY_KAFKA_URL`,
//! set by `tests/support/kafka-setup.sh`).

use kernway_kafka::KafkaBroker;
use kernway_messaging::conformance::{run_all, Harness};

#[derive(Clone)]
struct Kafka(String);

impl Harness for Kafka {
    type Broker = KafkaBroker;
    async fn broker(&self) -> KafkaBroker {
        KafkaBroker::connect_url(&self.0).await.unwrap_or_else(|e| panic!("connect to Kafka at {}: {e}", self.0))
    }
}

#[test]
fn kafka_conforms() {
    let Ok(url) = std::env::var("KERNWAY_KAFKA_URL") else {
        eprintln!("skip: KERNWAY_KAFKA_URL not set (tests/support/kafka-setup.sh)");
        return;
    };
    run_all(Kafka(url));
}
