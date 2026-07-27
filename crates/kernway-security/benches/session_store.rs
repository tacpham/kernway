#![allow(missing_docs, clippy::doc_overindented_list_items)] // a benchmark binary
//! The two primitives the auth path spends its time on, in one process.
//!
//! `authenticate` (KEP-0004) does two things of note per request: verify the
//! token, then look the session up in the store. This weighs them side by side:
//!
//!   - `get`    — the in-memory `SessionStore` lookup: a boxed future (the store
//!                is async so a Redis backend can await), driven to its ready
//!                result, holding a clone out of a 1000-entry map.
//!   - `verify` — the HMAC-SHA256 + base64url + JSON the auth path runs *before*
//!                it ever reaches the store.
//!
//! This is what settled the sync-vs-async-store question (KEP-0004): the store's
//! box is ~20 ns, a fraction of the ~1.6 µs `verify`, so making the store
//! uniformly async is free where it matters. The numbers live on so a regression
//! in either primitive shows up.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use kernway_security::session::{MemorySessionStore, SessionRecord, SessionStore};
use kernway_security::token::{Claims, TokenCodec};

const SID: &str = "8f14e45fceea167a5a36dedd4bea2543";
const KEY: &[u8] = b"a-32-byte-signing-key-for-hmac!!";

/// Drive a store future (a `Send` boxed future) to its ready value with a noop
/// waker — the in-memory store does no I/O, so it resolves on the first poll. The
/// future is already boxed by the trait, so this polls it directly: no extra box.
fn drive<T>(mut fut: Pin<Box<dyn Future<Output = T> + Send + '_>>) -> T {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => unreachable!("the in-memory store resolves on the first poll"),
    }
}

fn store() -> MemorySessionStore {
    let store = MemorySessionStore::new();
    let record = |user: String| SessionRecord {
        user,
        created: 1_700_000_000,
        last_seen: 1_700_000_000,
        meta: "chrome / 10.0.0.1".to_string(),
    };
    // A realistic registry size, so the lookup hashes and clones against a
    // populated map rather than a single entry.
    for i in 0..1000 {
        drive(store.insert(&format!("sid-{i:016x}"), record(format!("user{i}")))).unwrap();
    }
    drive(store.insert(SID, record("alice".to_string()))).unwrap();
    store
}

fn session_store(c: &mut Criterion) {
    let store = store();
    let codec = TokenCodec::new(KEY);
    let token = codec.sign(&Claims {
        sid: SID.to_string(),
        user: "alice".to_string(),
        roles: vec!["user".to_string()],
        version: 0,
        exp: 9_999_999_999,
    });

    let mut group = c.benchmark_group("session_store");

    // The async store lookup: box-allocate + poll + a clone out of the map.
    group.bench_function("get", |b| {
        b.iter(|| black_box(drive(store.get(black_box(SID)))).unwrap());
    });

    // What the auth path runs before it reaches the store: HMAC-SHA256 verify +
    // base64url decode + JSON claim parse.
    group.bench_function("verify", |b| {
        b.iter(|| black_box(codec.verify(black_box(&token))));
    });

    group.finish();
}

criterion_group!(benches, session_store);
criterion_main!(benches);
