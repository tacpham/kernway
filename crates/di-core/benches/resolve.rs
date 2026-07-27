#![allow(missing_docs)] // a benchmark binary, not public API
//! Bean resolution — the DI hot path.
//!
//! `AppContext::get` runs on every `#[inject]` field of every component built,
//! and (once controllers are wired) potentially per request. What it costs is
//! therefore worth knowing rather than assuming.
//!
//! The `passthrough_hasher_vs_siphash` group exists to test a claim made in a
//! comment in `context.rs` — that skipping SipHash over a `TypeId` is "a ~2×
//! win on the hot path". A comment is not evidence; this is.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use di_core::{AppContext, BeanEntry, BeanOrigin, Buildable, Container, DiError, RegistersComponent};

// A spread of distinct types, so lookups exercise a populated map rather than a
// single-entry one that lives entirely in cache.
macro_rules! filler_types {
    ($($name:ident),*) => { $( struct $name(#[allow(dead_code)] u64); )* };
}
filler_types!(T00, T01, T02, T03, T04, T05, T06, T07, T08, T09, T10, T11, T12, T13, T14, T15);

struct Repo;
struct Service {
    #[allow(dead_code)]
    repo: Arc<Repo>,
}

impl Buildable for Repo {
    fn build<C: Container + ?Sized>(_ctx: &C) -> Result<Arc<Self>, DiError> {
        Ok(Arc::new(Repo))
    }
}
impl RegistersComponent for Repo {}

impl Buildable for Service {
    fn build<C: Container + ?Sized>(ctx: &C) -> Result<Arc<Self>, DiError> {
        Ok(Arc::new(Service {
            repo: ctx.get::<Repo>()?,
        }))
    }
}
impl RegistersComponent for Service {
    fn dependencies() -> Vec<TypeId> {
        vec![TypeId::of::<Repo>()]
    }
}

trait Greeter: Send + Sync {
    fn greet(&self) -> u8;
}
struct English;
impl Greeter for English {
    fn greet(&self) -> u8 {
        1
    }
}

/// Context holding one target bean plus 16 others, i.e. a realistic small app.
fn populated_context() -> AppContext {
    let mut ctx = AppContext::new();
    ctx.register_instance::<Repo>(Arc::new(Repo)).unwrap();
    ctx.register_as::<dyn Greeter>(Arc::new(English)).unwrap();
    macro_rules! fill {
        ($($name:ident),*) => { $( ctx.register_instance::<$name>(Arc::new($name(0))).unwrap(); )* };
    }
    fill!(T00, T01, T02, T03, T04, T05, T06, T07, T08, T09, T10, T11, T12, T13, T14, T15);
    ctx
}

fn resolution(c: &mut Criterion) {
    let ctx = populated_context();

    let mut group = c.benchmark_group("resolve");
    group.bench_function("get_concrete", |b| {
        b.iter(|| black_box(ctx.get::<Repo>().unwrap()));
    });
    group.bench_function("get_as_trait", |b| {
        b.iter(|| black_box(ctx.get_as::<dyn Greeter>().unwrap().greet()));
    });
    group.bench_function("get_missing", |b| {
        // The error path matters too: a missing optional dependency takes it on
        // every `Option<Arc<T>>` field.
        b.iter(|| black_box(ctx.get::<String>().is_err()));
    });
    group.finish();
}

fn wiring(c: &mut Criterion) {
    let mut group = c.benchmark_group("refresh");
    // Startup cost: Kahn's algorithm over the declared graph. Runs once per
    // process, so this is about "does the app boot instantly", not throughput.
    group.bench_function("two_component_graph", |b| {
        b.iter_batched(
            || {
                let mut ctx = AppContext::new();
                ctx.register_component::<Service>()
                    .register_component::<Repo>();
                ctx
            },
            |mut ctx| {
                ctx.refresh().unwrap();
                black_box(ctx.bean_count())
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

// --- The claim under test ------------------------------------------------

/// Same map shape as `AppContext`, but with the default SipHash hasher — the
/// thing `TypeIdHasher` replaced.
type SipMap = HashMap<TypeId, Vec<(BeanEntry, Arc<dyn Any + Send + Sync>)>>;

/// A local copy of the passthrough hasher, so both sides can be measured in one
/// process. Kept byte-identical to `context.rs`; if that changes, this must too.
#[derive(Default)]
struct TypeIdHasher {
    hash: u64,
}

impl std::hash::Hasher for TypeIdHasher {
    fn finish(&self) -> u64 {
        self.hash
    }
    fn write_u64(&mut self, i: u64) {
        self.hash = i;
    }
    fn write_u128(&mut self, i: u128) {
        self.hash = (i as u64) ^ ((i >> 64) as u64);
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.hash = self.hash.rotate_left(8) ^ u64::from(b);
        }
    }
}

type FastMap = HashMap<TypeId, Vec<(BeanEntry, Arc<dyn Any + Send + Sync>)>, BuildHasherDefault<TypeIdHasher>>;

fn fill_keys() -> Vec<TypeId> {
    vec![
        TypeId::of::<Repo>(),
        TypeId::of::<T00>(), TypeId::of::<T01>(), TypeId::of::<T02>(), TypeId::of::<T03>(),
        TypeId::of::<T04>(), TypeId::of::<T05>(), TypeId::of::<T06>(), TypeId::of::<T07>(),
        TypeId::of::<T08>(), TypeId::of::<T09>(), TypeId::of::<T10>(), TypeId::of::<T11>(),
        TypeId::of::<T12>(), TypeId::of::<T13>(), TypeId::of::<T14>(), TypeId::of::<T15>(),
    ]
}

fn hasher_comparison(c: &mut Criterion) {
    let keys = fill_keys();
    let entry = || {
        (
            BeanEntry::new(TypeId::of::<Repo>(), "Repo", BeanOrigin::User),
            Arc::new(Repo) as Arc<dyn Any + Send + Sync>,
        )
    };

    let mut sip: SipMap = HashMap::default();
    let mut fast: FastMap = FastMap::default();
    for key in &keys {
        sip.insert(*key, vec![entry()]);
        fast.insert(*key, vec![entry()]);
    }

    let mut group = c.benchmark_group("passthrough_hasher_vs_siphash");
    // One lookup of every registered type — what building a component with many
    // injected fields does.
    group.bench_function("siphash_17_lookups", |b| {
        b.iter(|| {
            for key in &keys {
                black_box(sip.get(black_box(key)));
            }
        });
    });
    group.bench_function("passthrough_17_lookups", |b| {
        b.iter(|| {
            for key in &keys {
                black_box(fast.get(black_box(key)));
            }
        });
    });
    group.finish();
}

criterion_group!(benches, resolution, wiring, hasher_comparison);
criterion_main!(benches);
