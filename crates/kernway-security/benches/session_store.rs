//! Sync vs async in-memory `SessionStore`, in the context that decides it.
//!
//! Making `SessionStore` async (so a Redis backend can `.await`) costs the
//! in-memory store a boxed future per call, even though it never blocks. The
//! question (KEP-0004): is that box a real cost on the auth path, or noise next
//! to what `authenticate` already pays?
//!
//! So three numbers, same machine, same process:
//!   - `get_sync`      — the store lookup as it is today (a clone out of a map).
//!   - `get_async`     — the same lookup wrapped in `Box::pin(async { … })` and
//!                       driven to its ready result: exactly what an async trait
//!                       would add per call.
//!   - `verify`        — the HMAC-SHA256 + base64 + JSON the auth path runs on
//!                       *every* request before it ever touches the store.
//!
//! If `get_async - get_sync` is small next to `verify`, the box is noise on the
//! path that matters, and a uniformly-async store is free in practice.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use kernway_security::session::{MemorySessionStore, SessionRecord, SessionStore};
use kernway_security::token::{Claims, TokenCodec};

const SID: &str = "8f14e45fceea167a5a36dedd4bea2543";
const KEY: &[u8] = b"a-32-byte-signing-key-for-hmac!!";

fn store() -> MemorySessionStore {
    let store = MemorySessionStore::new();
    // A realistic registry size, so the lookup hashes and clones against a
    // populated map rather than a single entry.
    for i in 0..1000 {
        store.insert(
            &format!("sid-{i:016x}"),
            SessionRecord {
                user: format!("user{i}"),
                created: 1_700_000_000,
                last_seen: 1_700_000_000,
                meta: "chrome / 10.0.0.1".to_string(),
            },
        );
    }
    store.insert(
        SID,
        SessionRecord {
            user: "alice".to_string(),
            created: 1_700_000_000,
            last_seen: 1_700_000_000,
            meta: "chrome / 10.0.0.1".to_string(),
        },
    );
    store
}

/// Drive an immediately-ready future to its value with a noop waker — the store
/// does no real I/O, so it resolves on the first poll. This is the box-allocate
/// + poll an async trait method would add.
fn drive<T>(mut fut: Pin<Box<dyn Future<Output = T> + '_>>) -> T {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => unreachable!("the in-memory store resolves on the first poll"),
    }
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

    // The store lookup as it stands: sync, a clone out of the map.
    group.bench_function("get_sync", |b| {
        b.iter(|| black_box(store.get(black_box(SID))));
    });

    // The same lookup, but returned as a boxed future and driven to ready —
    // the exact shape (and cost) an async `SessionStore` method would have.
    group.bench_function("get_async", |b| {
        b.iter(|| {
            let sid = SID;
            let fut: Pin<Box<dyn Future<Output = Option<SessionRecord>> + '_>> =
                Box::pin(async { store.get(sid) });
            black_box(drive(fut))
        });
    });

    // What the auth path runs before it ever reaches the store: HMAC-SHA256
    // verify + base64url decode + JSON claim parse. The weight to compare the
    // box against.
    group.bench_function("verify", |b| {
        b.iter(|| black_box(codec.verify(black_box(&token))));
    });

    group.finish();
}

criterion_group!(benches, session_store);
criterion_main!(benches);
