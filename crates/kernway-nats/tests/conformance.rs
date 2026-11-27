//! The messaging conformance suite against a live NATS server with
//! JetStream (`KERNWAY_NATS_URL`, default `nats://127.0.0.1:4222`).

use kernway_messaging::conformance::{run_all, Harness};
use kernway_nats::NatsBroker;

/// The servers are up (`tests/support/nats-setup.sh` printed the env);
/// otherwise the test says `skip:` — the CI job and the acceptance run
/// count that as a failure.
fn live() -> bool {
    let up = std::env::var_os("KERNWAY_NATS_DIR").is_some();
    if !up {
        eprintln!("skip: KERNWAY_NATS_DIR not set (tests/support/nats-setup.sh)");
    }
    up
}


fn url() -> String {
    std::env::var("KERNWAY_NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

#[derive(Clone)]
struct Nats;

impl Harness for Nats {
    type Broker = NatsBroker;
    async fn broker(&self) -> NatsBroker {
        NatsBroker::connect_url(&url()).await.unwrap_or_else(|e| panic!("connect to NATS at {}: {e}", url()))
    }
}

#[test]
fn nats_jetstream_conforms() {
    if !live() {
        return;
    }
    run_all(Nats);
}
