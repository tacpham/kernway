//! Per-shard (thread-local) free-list of byte buffers for connection I/O.
//!
//! Each shard is one OS thread with its own executor (`rt-net`'s shared-nothing
//! model), so a `thread_local!` free-list is per-shard and needs no lock. A
//! connection takes its two read buffers from the pool on start; a RAII guard
//! returns them on drop — on every exit path, including a panic — so steady-state
//! connection churn reuses buffers instead of allocating a fresh pair each time.
//!
//! Both bounds matter: the pool holds at most [`MAX_POOLED`] free buffers per
//! shard (so idle memory is capped), and a buffer that grew past
//! [`MAX_POOLED_CAP`] (a large head or in-memory body ballooned `buf`) is dropped
//! rather than returned (so the pool never pins a big allocation).

use std::cell::RefCell;

/// Free buffers kept per shard before extras are dropped.
const MAX_POOLED: usize = 512;

/// A buffer bigger than this is dropped instead of pooled.
const MAX_POOLED_CAP: usize = 64 * 1024;

thread_local! {
    static POOL: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

/// Pull a buffer off the shard's free-list (or allocate one), cleared and with at
/// least `min_cap` capacity.
fn take(min_cap: usize) -> Vec<u8> {
    match POOL.with(|p| p.borrow_mut().pop()) {
        Some(mut v) => {
            v.clear();
            if v.capacity() < min_cap {
                v.reserve(min_cap - v.capacity());
            }
            v
        }
        None => Vec::with_capacity(min_cap),
    }
}

/// Return a buffer to the shard's free-list, unless it never allocated, grew too
/// big, or the pool is already full.
fn give(mut v: Vec<u8>) {
    let cap = v.capacity();
    if cap == 0 || cap > MAX_POOLED_CAP {
        return;
    }
    v.clear();
    POOL.with(|p| {
        let mut pool = p.borrow_mut();
        if pool.len() < MAX_POOLED {
            pool.push(v);
        }
    });
}

/// A pooled byte buffer that returns itself to the shard's free-list on drop.
///
/// Derefs to `Vec<u8>`, so it is a drop-in for the `Vec<u8>` the connection loop
/// used before — `extend_from_slice`, indexing, `drain`, and `&buf`/`&mut buf`
/// coercions all work through the deref.
pub(crate) struct PooledBuf(Option<Vec<u8>>);

impl PooledBuf {
    /// An empty buffer with at least `min_cap` capacity — for accumulation (`buf`).
    pub(crate) fn cleared(min_cap: usize) -> Self {
        PooledBuf(Some(take(min_cap)))
    }

    /// A buffer of exactly `len` zero bytes — a fixed-size read scratch (`chunk`).
    pub(crate) fn filled(len: usize) -> Self {
        let mut v = take(len);
        v.resize(len, 0);
        PooledBuf(Some(v))
    }
}

impl std::ops::Deref for PooledBuf {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        self.0.as_ref().expect("PooledBuf used after drop")
    }
}

impl std::ops::DerefMut for PooledBuf {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        self.0.as_mut().expect("PooledBuf used after drop")
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        if let Some(v) = self.0.take() {
            give(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_the_same_allocation_across_takes() {
        // Drain any buffers a prior test left on this thread's pool.
        POOL.with(|p| p.borrow_mut().clear());
        let ptr = {
            let mut b = PooledBuf::cleared(1024);
            b.extend_from_slice(b"hello");
            b.as_ptr()
        }; // returned to the pool here
        let b2 = PooledBuf::cleared(1024);
        assert_eq!(b2.len(), 0, "a reused buffer comes back cleared");
        assert_eq!(b2.as_ptr(), ptr, "the same allocation is handed back");
    }

    #[test]
    fn filled_gives_requested_length_zeroed() {
        POOL.with(|p| p.borrow_mut().clear());
        let b = PooledBuf::filled(8);
        assert_eq!(&b[..], &[0u8; 8]);
    }

    #[test]
    fn oversized_buffers_are_not_pooled() {
        POOL.with(|p| p.borrow_mut().clear());
        {
            let mut b = PooledBuf::cleared(16);
            b.reserve(MAX_POOLED_CAP + 1); // grow past the cap
            let _ = b.capacity();
        } // dropped — must NOT go back to the pool
        assert_eq!(POOL.with(|p| p.borrow().len()), 0);
    }
}
