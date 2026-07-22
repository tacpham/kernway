//! Reactor — I/O readiness on top of `mio::Poll` (epoll / kqueue / IOCP).
//!
//! One reactor per shard. It owns the readiness registry: a source registers
//! once and gets a [`Token`]; each time a future on that source returns
//! `Pending` it parks its [`Waker`] here, and [`Reactor::poll`] hands the waker
//! back when the OS reports readiness.

use std::collections::HashMap;
use std::io;
use std::task::Waker;
use std::time::Duration;

use mio::{event::Source, Events, Interest, Poll, Registry, Token};

/// Reserved for the cross-thread unparker — never dispatched to a task waker.
pub(crate) const UNPARK_TOKEN: Token = Token(0);

/// Which half of a source a future is waiting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Readable / incoming connection.
    Read,
    /// Writable / connect completed.
    Write,
}

/// Wakers parked on one registered source.
#[derive(Default)]
struct Parked {
    read: Option<Waker>,
    write: Option<Waker>,
}

/// I/O event demultiplexer for a single shard.
pub struct Reactor {
    poll: Poll,
    events: Events,
    parked: HashMap<Token, Parked>,
    /// Token 0 is the unparker, so real sources start at 1.
    next_token: usize,
}

impl Reactor {
    /// Create a reactor with the given event-buffer capacity.
    pub fn with_capacity(capacity: usize) -> io::Result<Self> {
        Ok(Self {
            poll: Poll::new()?,
            events: Events::with_capacity(capacity),
            parked: HashMap::new(),
            next_token: UNPARK_TOKEN.0 + 1,
        })
    }

    /// Create a reactor with a 1024-event buffer.
    pub fn new() -> io::Result<Self> {
        Self::with_capacity(1024)
    }

    /// The underlying registry — used to build the shard's unparker.
    pub fn registry(&self) -> &Registry {
        self.poll.registry()
    }

    /// Register an I/O source and return the token identifying it.
    ///
    /// Registers for read **and** write readiness: a socket is typically read
    /// and written by the same task, and re-registering per direction costs an
    /// extra syscall per switch.
    pub fn register<S: Source + ?Sized>(&mut self, source: &mut S) -> io::Result<Token> {
        let token = Token(self.next_token);
        self.next_token += 1;
        self.poll
            .registry()
            .register(source, token, Interest::READABLE | Interest::WRITABLE)?;
        self.parked.insert(token, Parked::default());
        Ok(token)
    }

    /// Drop a source's registration and any wakers parked on it.
    ///
    /// The token is not recycled — `next_token` only moves forward, so a stale
    /// event can never be delivered to a later source that reused the number.
    pub fn deregister<S: Source + ?Sized>(&mut self, source: &mut S, token: Token) -> io::Result<()> {
        self.parked.remove(&token);
        self.poll.registry().deregister(source)
    }

    /// Park `waker` until `token` is ready in `direction`.
    ///
    /// Replacing an existing waker is correct and expected: only the most recent
    /// poll of a given direction matters, and `Waker::will_wake` lets callers
    /// skip the store when nothing changed.
    pub fn park(&mut self, token: Token, direction: Direction, waker: Waker) {
        let slot = self.parked.entry(token).or_default();
        let target = match direction {
            Direction::Read => &mut slot.read,
            Direction::Write => &mut slot.write,
        };
        match target {
            Some(existing) if existing.will_wake(&waker) => {}
            _ => *target = Some(waker),
        }
    }

    /// Number of sources currently registered — the executor uses this to tell
    /// "waiting for I/O" apart from "nothing can ever wake us".
    pub fn source_count(&self) -> usize {
        self.parked.len()
    }

    /// Wait for readiness (until `timeout`, or indefinitely if `None`) and wake
    /// every task whose source became ready.
    ///
    /// A `WouldBlock`/`Interrupted` return from the OS is normal (a signal, or a
    /// spurious wakeup) and is reported as success with nothing woken.
    pub fn poll(&mut self, timeout: Option<Duration>) -> io::Result<usize> {
        match self.poll.poll(&mut self.events, timeout) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return Ok(0),
            Err(e) => return Err(e),
        }

        let mut woken = 0;
        for event in self.events.iter() {
            let token = event.token();
            if token == UNPARK_TOKEN {
                continue; // the cross-thread unparker; the queue drain handles it
            }
            let Some(slot) = self.parked.get_mut(&token) else {
                continue; // source deregistered between poll and dispatch
            };
            // An error/HUP wakes both halves so the task observes the failure on
            // whichever side it happens to be waiting.
            let failed = event.is_error() || event.is_read_closed() || event.is_write_closed();
            if event.is_readable() || failed {
                if let Some(w) = slot.read.take() {
                    w.wake();
                    woken += 1;
                }
            }
            if event.is_writable() || failed {
                if let Some(w) = slot.write.take() {
                    w.wake();
                    woken += 1;
                }
            }
        }
        Ok(woken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Counts how many times it was woken.
    fn counting_waker(hits: Arc<AtomicUsize>) -> Waker {
        use std::task::{RawWaker, RawWakerVTable};

        unsafe fn clone(ptr: *const ()) -> RawWaker {
            // SAFETY: `ptr` is a live `Arc<AtomicUsize>` pointer from `into_raw`.
            unsafe { Arc::increment_strong_count(ptr as *const AtomicUsize) };
            RawWaker::new(ptr, &VT)
        }
        unsafe fn wake(ptr: *const ()) {
            // SAFETY: as above; consumes the reference this waker owned.
            let hits = unsafe { Arc::from_raw(ptr as *const AtomicUsize) };
            hits.fetch_add(1, Ordering::SeqCst);
        }
        unsafe fn wake_by_ref(ptr: *const ()) {
            // SAFETY: as above; borrowed, so the count is left untouched.
            let hits = unsafe { &*(ptr as *const AtomicUsize) };
            hits.fetch_add(1, Ordering::SeqCst);
        }
        unsafe fn drop_it(ptr: *const ()) {
            // SAFETY: as above; releases this waker's reference.
            drop(unsafe { Arc::from_raw(ptr as *const AtomicUsize) });
        }
        static VT: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_it);

        let ptr = Arc::into_raw(hits) as *const ();
        // SAFETY: `ptr` is a fresh `Arc<AtomicUsize>` leak matching VT above.
        unsafe { Waker::from_raw(RawWaker::new(ptr, &VT)) }
    }

    #[test]
    fn tokens_start_after_the_unparker_and_never_repeat() {
        let mut reactor = Reactor::new().unwrap();
        let mut a = mio::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut b = mio::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let ta = reactor.register(&mut a).unwrap();
        let tb = reactor.register(&mut b).unwrap();
        assert_ne!(ta, UNPARK_TOKEN);
        assert_ne!(ta, tb);

        // Deregistering must not recycle the token — a stale event would then be
        // delivered to whichever source got the number next.
        reactor.deregister(&mut a, ta).unwrap();
        let mut c = mio::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        assert_ne!(reactor.register(&mut c).unwrap(), ta);
    }

    #[test]
    fn re_parking_keeps_one_waker_and_the_newest_wins() {
        let mut reactor = Reactor::new().unwrap();
        let token = Token(1);
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));

        // Same waker twice → the stored one is left in place (the `will_wake`
        // fast path), and only ever one waker is held per direction.
        let waker = counting_waker(Arc::clone(&first));
        reactor.park(token, Direction::Read, waker.clone());
        reactor.park(token, Direction::Read, waker.clone());
        assert!(reactor.parked[&token].read.as_ref().unwrap().will_wake(&waker));

        // A different waker replaces it: the task re-polled, and only the most
        // recent poll's waker may be woken.
        reactor.park(token, Direction::Read, counting_waker(Arc::clone(&second)));
        reactor.parked.get_mut(&token).unwrap().read.take().unwrap().wake();
        assert_eq!(first.load(Ordering::SeqCst), 0, "the stale waker must not fire");
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn read_and_write_wakers_are_tracked_separately() {
        let mut reactor = Reactor::new().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let token = Token(1);
        reactor.park(token, Direction::Read, counting_waker(Arc::clone(&hits)));
        reactor.park(token, Direction::Write, counting_waker(Arc::clone(&hits)));
        let slot = &reactor.parked[&token];
        assert!(slot.read.is_some() && slot.write.is_some());
    }

    #[test]
    fn readable_source_wakes_its_parked_task() {
        use std::io::Write;
        use std::net::TcpStream;

        let mut reactor = Reactor::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let mut client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut server = mio::net::TcpStream::from_std(server);

        let token = reactor.register(&mut server).unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        reactor.park(token, Direction::Read, counting_waker(Arc::clone(&hits)));

        client.write_all(b"ping").unwrap();
        // Bounded retries: readiness may need more than one poll to surface.
        for _ in 0..50 {
            reactor.poll(Some(Duration::from_millis(100))).unwrap();
            if hits.load(Ordering::SeqCst) > 0 {
                break;
            }
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "incoming data must wake the reader");
    }

    #[test]
    fn poll_with_no_sources_times_out_cleanly() {
        let mut reactor = Reactor::new().unwrap();
        assert_eq!(reactor.poll(Some(Duration::from_millis(1))).unwrap(), 0);
    }

    #[test]
    fn events_for_an_unknown_token_are_ignored() {
        // Deregistered mid-flight: dispatch must skip it, not panic.
        let mut reactor = Reactor::new().unwrap();
        let mut listener = mio::net::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let token = reactor.register(&mut listener).unwrap();
        reactor.deregister(&mut listener, token).unwrap();
        assert_eq!(reactor.source_count(), 0);
        assert_eq!(reactor.poll(Some(Duration::from_millis(1))).unwrap(), 0);
    }
}
