//! A one-value channel from a worker thread to an async task: the worker
//! [`send`](Sender::send)s, the task awaits the [`Receiver`]. A sender
//! dropped without sending (the worker died) completes the receiver with
//! `None`, so a task never waits forever.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

struct Shared<T> {
    value: Option<T>,
    waker: Option<Waker>,
    closed: bool,
}

pub(crate) struct Sender<T>(Arc<Mutex<Shared<T>>>);
pub(crate) struct Receiver<T>(Arc<Mutex<Shared<T>>>);

pub(crate) fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let s = Arc::new(Mutex::new(Shared { value: None, waker: None, closed: false }));
    (Sender(Arc::clone(&s)), Receiver(s))
}

impl<T> Sender<T> {
    pub(crate) fn send(self, value: T) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).value = Some(value);
        // Drop closes and wakes.
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
            s.closed = true;
            s.waker.take()
        };
        // Wake outside the lock: the task may poll at once on another thread.
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> Future for Receiver<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if s.closed {
            return Poll::Ready(s.value.take());
        }
        s.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
