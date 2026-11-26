//! The cache attributes are markers for now (see the crate docs): whatever they
//! annotate — free functions, methods, `async fn`, generics, with or without
//! arguments — must compile and behave exactly as if unannotated. The last test
//! pins down that a `#[cacheable]` function is **not** cached yet, so the day
//! the real caching lands this test is the one that has to change.

use std::cell::Cell;

use kernway_cache_macro::{cache_evict, cache_update, cacheable};

thread_local! {
    static CALLS: Cell<u32> = const { Cell::new(0) };
}

fn calls() -> u32 {
    CALLS.with(Cell::get)
}

#[cacheable(key = "square_{n}", ttl = 60, region = "math")]
fn square(n: u64) -> u64 {
    CALLS.with(|c| c.set(c.get() + 1));
    n * n
}

#[cacheable]
/// Docs and other attributes on the item survive.
#[must_use]
pub fn bare<T: Clone>(v: &T) -> T {
    v.clone()
}

struct Users {
    names: Vec<String>,
}

impl Users {
    #[cacheable(key = "user_{id}")]
    fn get(&self, id: usize) -> Option<&str> {
        self.names.get(id).map(String::as_str)
    }

    #[cache_update(key = "user_{id}", ttl = 30)]
    fn rename(&mut self, id: usize, name: &str) -> String {
        self.names[id] = name.to_string();
        self.names[id].clone()
    }

    #[cache_evict(key = "user_{id}")]
    fn remove(&mut self, id: usize) {
        self.names.remove(id);
    }

    #[cache_evict(all_entries = true, region = "users")]
    fn clear(&mut self) -> usize {
        let n = self.names.len();
        self.names.clear();
        n
    }

    #[cacheable(key = "count")]
    async fn count(&self) -> usize {
        self.names.len()
    }
}

#[test]
fn annotated_functions_compile_and_run_unchanged() {
    assert_eq!(bare(&"x".to_string()), "x");
    let mut users = Users { names: vec!["ann".into(), "bob".into(), "cy".into()] };
    assert_eq!(users.get(1), Some("bob"));
    assert_eq!(users.rename(1, "bo"), "bo");
    assert_eq!(users.get(1), Some("bo"), "the update really happened");
    users.remove(0);
    assert_eq!(users.get(0), Some("bo"));
    let n = rt_core::Executor::new().unwrap().block_on(users.count()).unwrap();
    assert_eq!(n, 2);
    assert_eq!(users.clear(), 2);
    assert_eq!(users.get(0), None);
}

#[test]
fn cacheable_is_a_marker_the_body_runs_every_call() {
    let before = calls();
    assert_eq!(square(7), 49);
    assert_eq!(square(7), 49);
    assert_eq!(calls() - before, 2, "no caching yet: the body ran on both calls");
}
