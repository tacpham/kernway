//! The messaging conformance suite against a live RabbitMQ
//! (`KERNWAY_RABBITMQ_URL`, set by `tests/support/rabbitmq-setup.sh`).

use kernway_messaging::conformance::{run_all, Harness};
use kernway_rabbitmq::RabbitBroker;

fn url() -> Option<String> {
    let u = std::env::var("KERNWAY_RABBITMQ_URL").ok();
    if u.is_none() {
        eprintln!("skip: KERNWAY_RABBITMQ_URL not set (tests/support/rabbitmq-setup.sh)");
    }
    u
}

#[derive(Clone)]
struct Rabbit(String);

impl Harness for Rabbit {
    type Broker = RabbitBroker;
    async fn broker(&self) -> RabbitBroker {
        RabbitBroker::connect_uri(&self.0).await.unwrap_or_else(|e| panic!("connect to RabbitMQ at {}: {e}", self.0))
    }
}

#[test]
fn rabbitmq_conforms() {
    let Some(u) = url() else { return };
    run_all(Rabbit(u));
}
