//! Shared test helpers: the deployments of `tests/support/mongo-setup.sh`.
#![allow(dead_code)]

use std::future::Future;

pub fn block<F: Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

/// `name` from the environment; `None` (with a `skip:` line) when unset.
pub fn env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => Some(v),
        Err(_) => {
            eprintln!("skip: {name} not set (tests/support/mongo-setup.sh)");
            None
        }
    }
}

/// A unique collection name per test run.
pub fn unique(name: &str) -> String {
    format!("{name}_{}_{}", std::process::id(), &kernway_orm_mongodb::ObjectId::new().to_hex()[18..])
}
