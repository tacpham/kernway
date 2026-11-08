//! A minimal, per-core async connection pool.
//!
//! Thread-per-core means no cross-thread sharing: the pool is `!Send`, built on
//! `Rc` + `RefCell` with **no locks and no atomics**. `acquire().await` hands out
//! a connection (opening one up to `max`, or parking on a waker until one is
//! released). Each in-flight operation owns its connection exclusively, so there
//! is no shared-mutable-connection hazard across await points.

use crate::protocol::{Conn, MysqlConfig};
use kernway_orm_core::error::OrmError;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// A per-core pool of MySQL connections.
pub struct Pool {
    cfg: MysqlConfig,
    idle: RefCell<Vec<Conn>>,
    waiters: RefCell<VecDeque<Waker>>,
    open: Cell<usize>,
    max: usize,
}

impl Pool {
    /// Create an empty pool that opens up to `max` connections on demand.
    pub fn new(cfg: MysqlConfig, max: usize) -> Rc<Self> {
        Rc::new(Self {
            cfg,
            idle: RefCell::new(Vec::new()),
            waiters: RefCell::new(VecDeque::new()),
            open: Cell::new(0),
            max: max.max(1),
        })
    }

    /// Open the first connection eagerly so a bad URL / auth fails fast at setup.
    pub async fn warm(self: &Rc<Self>) -> Result<(), OrmError> {
        let conn = self.acquire().await?;
        drop(conn);
        Ok(())
    }

    /// Acquire a connection: reuse an idle one, open a new one (up to `max`), or
    /// park until a release wakes us.
    pub async fn acquire(self: &Rc<Self>) -> Result<PooledConn, OrmError> {
        loop {
            if let Some(conn) = self.idle.borrow_mut().pop() {
                return Ok(PooledConn { conn: Some(conn), pool: Rc::clone(self), healthy: true });
            }
            if self.open.get() < self.max {
                self.open.set(self.open.get() + 1);
                match Conn::connect(&self.cfg).await {
                    Ok(conn) => {
                        return Ok(PooledConn { conn: Some(conn), pool: Rc::clone(self), healthy: true })
                    }
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

/// A connection checked out of the pool; returned (or discarded if poisoned) on
/// drop. Derefs to [`Conn`].
pub struct PooledConn {
    conn: Option<Conn>,
    pool: Rc<Pool>,
    healthy: bool,
}

impl PooledConn {
    /// Mark the connection unusable (dead socket / protocol desync) so it is
    /// discarded instead of returned to the pool.
    pub fn poison(&mut self) {
        self.healthy = false;
    }
}

impl Deref for PooledConn {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        self.conn.as_ref().expect("pooled connection present")
    }
}
impl DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Conn {
        self.conn.as_mut().expect("pooled connection present")
    }
}

impl Drop for PooledConn {
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
struct WaitForSlot {
    pool: Rc<Pool>,
}

impl Future for WaitForSlot {
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
