//! A minimal, per-core async connection pool, generic over the driver.
//!
//! Thread-per-core means no cross-thread sharing: the pool is `!Send`, built on
//! `Rc` + `RefCell` with **no locks and no atomics**. `acquire().await` hands out
//! a connection (opening one up to `max`, or parking on a waker until one is
//! released). Each in-flight operation owns its connection exclusively, so there
//! is no shared-mutable-connection hazard across await points. A connection the
//! driver marks broken ([`Pooled::poison`]) is dropped instead of reused.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// How a driver opens one connection for the [`Pool`].
pub trait Connect {
    /// The driver's connection type.
    type Conn;
    /// The driver's error type.
    type Error;
    /// Open and authenticate one connection.
    fn connect(&self) -> impl Future<Output = Result<Self::Conn, Self::Error>>;
}

/// A per-core pool of connections opened by `C`.
pub struct Pool<C: Connect> {
    connector: C,
    idle: RefCell<Vec<C::Conn>>,
    waiters: RefCell<VecDeque<Waker>>,
    open: Cell<usize>,
    max: usize,
}

impl<C: Connect> Pool<C> {
    /// Create an empty pool that opens up to `max` connections (at least one)
    /// on demand.
    pub fn new(connector: C, max: usize) -> Rc<Self> {
        Rc::new(Self {
            connector,
            idle: RefCell::new(Vec::new()),
            waiters: RefCell::new(VecDeque::new()),
            open: Cell::new(0),
            max: max.max(1),
        })
    }

    /// The connector this pool opens connections with.
    pub fn connector(&self) -> &C {
        &self.connector
    }

    /// Open the first connection eagerly so a bad address or credentials fail
    /// fast at setup.
    pub async fn warm(self: &Rc<Self>) -> Result<(), C::Error> {
        drop(self.acquire().await?);
        Ok(())
    }

    /// Connections currently open (idle or checked out).
    pub fn open_connections(&self) -> usize {
        self.open.get()
    }

    /// Acquire a connection: reuse an idle one, open a new one (up to `max`), or
    /// park until a release wakes us.
    pub async fn acquire(self: &Rc<Self>) -> Result<Pooled<C>, C::Error> {
        loop {
            if let Some(conn) = self.idle.borrow_mut().pop() {
                return Ok(Pooled::new(conn, self));
            }
            if self.open.get() < self.max {
                self.open.set(self.open.get() + 1);
                match self.connector.connect().await {
                    Ok(conn) => return Ok(Pooled::new(conn, self)),
                    Err(e) => {
                        self.open.set(self.open.get() - 1);
                        self.wake_one(); // free the grow slot for a waiter to retry
                        return Err(e);
                    }
                }
            }
            WaitForSlot { pool: Rc::clone(self) }.await;
        }
    }

    fn wake_one(&self) {
        if let Some(waker) = self.waiters.borrow_mut().pop_front() {
            waker.wake();
        }
    }
}

/// A connection checked out of a [`Pool`]; returned on drop, or discarded if
/// [`poison`](Self::poison)ed. Derefs to the driver's connection.
pub struct Pooled<C: Connect> {
    conn: Option<C::Conn>,
    pool: Rc<Pool<C>>,
    healthy: bool,
}

impl<C: Connect> Pooled<C> {
    fn new(conn: C::Conn, pool: &Rc<Pool<C>>) -> Self {
        Self { conn: Some(conn), pool: Rc::clone(pool), healthy: true }
    }

    /// Mark the connection unusable (dead socket, protocol desync) so it is
    /// discarded instead of returned to the pool.
    pub fn poison(&mut self) {
        self.healthy = false;
    }

    /// Mark the connection fit to return to the pool. With
    /// [`poison`](Self::poison) this brackets an operation: poison before it
    /// starts, mark healthy once it has completed — a future dropped half-way
    /// (a timeout, a cancelled request) then discards the connection instead
    /// of pooling one with a half-read reply or an open transaction.
    pub fn mark_healthy(&mut self) {
        self.healthy = true;
    }
}

impl<C: Connect> Deref for Pooled<C> {
    type Target = C::Conn;
    fn deref(&self) -> &C::Conn {
        self.conn.as_ref().expect("pooled connection present")
    }
}

impl<C: Connect> DerefMut for Pooled<C> {
    fn deref_mut(&mut self) -> &mut C::Conn {
        self.conn.as_mut().expect("pooled connection present")
    }
}

impl<C: Connect> Drop for Pooled<C> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if self.healthy {
                self.pool.idle.borrow_mut().push(conn);
            } else {
                self.pool.open.set(self.pool.open.get().saturating_sub(1));
            }
            self.pool.wake_one();
        }
    }
}

/// Parks until a connection slot (idle or growable) is available.
struct WaitForSlot<C: Connect> {
    pool: Rc<Pool<C>>,
}

impl<C: Connect> Future for WaitForSlot<C> {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let pool = &self.pool;
        if !pool.idle.borrow().is_empty() || pool.open.get() < pool.max {
            Poll::Ready(())
        } else {
            pool.waiters.borrow_mut().push_back(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connector handing out numbered fake connections; fails on demand.
    struct Fake {
        next: Cell<u32>,
        fail: Cell<bool>,
    }

    impl Connect for Fake {
        type Conn = u32;
        type Error = &'static str;
        async fn connect(&self) -> Result<u32, &'static str> {
            if self.fail.get() {
                return Err("refused");
            }
            self.next.set(self.next.get() + 1);
            Ok(self.next.get())
        }
    }

    fn pool(max: usize) -> Rc<Pool<Fake>> {
        Pool::new(Fake { next: Cell::new(0), fail: Cell::new(false) }, max)
    }

    fn block<F: Future<Output = ()>>(f: F) {
        rt_core::Executor::new().unwrap().block_on(f).unwrap();
    }

    #[test]
    fn a_released_connection_is_reused_and_a_poisoned_one_is_not() {
        block(async {
            let p = pool(4);
            let first = *p.acquire().await.unwrap();
            assert_eq!(*p.acquire().await.unwrap(), first, "returned to the pool and reused");
            let mut c = p.acquire().await.unwrap();
            c.poison();
            drop(c);
            assert_eq!(p.open_connections(), 0, "poisoned connection discarded");
            assert_ne!(*p.acquire().await.unwrap(), first, "a fresh one is opened");
        });
    }

    #[test]
    fn the_pool_never_opens_more_than_max() {
        block(async {
            let p = pool(2);
            let a = p.acquire().await.unwrap();
            let b = p.acquire().await.unwrap();
            assert_eq!(p.open_connections(), 2);
            drop(a);
            let c = p.acquire().await.unwrap();
            assert_eq!(p.open_connections(), 2, "waits for / reuses a slot instead of growing");
            drop((b, c));
        });
    }

    #[test]
    fn a_failed_connect_frees_its_slot() {
        block(async {
            let p = pool(1);
            p.connector().fail.set(true);
            assert!(p.acquire().await.is_err());
            assert_eq!(p.open_connections(), 0);
            p.connector().fail.set(false);
            assert!(p.acquire().await.is_ok());
        });
    }
}
