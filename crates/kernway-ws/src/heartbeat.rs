//! Per-shard heartbeat: **one** timer task per core drives pings and idle
//! detection for every WebSocket on that core.
//!
//! The naive design — a `timeout` around every `read`, plus a ping task per
//! connection — puts a timer insert *and* remove on the hot path of every
//! message and keeps tens of thousands of timers alive. Here each connection
//! only stamps a shard-local **coarse clock** when it reads (a `Cell` load, no
//! syscall); a single sweeper wakes every `tick`, refreshes that clock once, and
//! walks the connections: ping the ones that are due, close the ones that went
//! quiet. Like Netty's `IdleStateHandler`, but batched per core.
//!
//! The sweeper exits when its core has no connections left and is restarted by
//! the next one, so an idle shard has no timer at all.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// A connection the sweeper looks after.
pub(crate) trait Beat {
    /// Called once per tick with the coarse `now`. Return `false` to be dropped
    /// from the registry (the connection is finished).
    fn beat(&self, now: Instant) -> bool;
}

struct Registry {
    conns: RefCell<Vec<Weak<dyn Beat>>>,
    tick: Cell<Duration>,
    running: Cell<bool>,
}

thread_local! {
    static REGISTRY: Rc<Registry> = Rc::new(Registry {
        conns: RefCell::new(Vec::new()),
        tick: Cell::new(MAX_TICK),
        running: Cell::new(false),
    });
    /// The shard's coarse clock — refreshed by the sweeper (and on registration).
    static NOW: Cell<Instant> = Cell::new(Instant::now());
}

const MIN_TICK: Duration = Duration::from_millis(10);
const MAX_TICK: Duration = Duration::from_secs(1);

/// The shard's coarse "now": accurate to one tick, costs no syscall.
pub(crate) fn coarse_now() -> Instant {
    NOW.with(Cell::get)
}

/// Look after `conn` on this shard. `finest` is the shortest period it needs
/// observed (its ping interval or idle timeout); the tick is a quarter of the
/// finest period registered, clamped to [10 ms, 1 s].
pub(crate) fn register(conn: Weak<dyn Beat>, finest: Duration) {
    NOW.with(|n| n.set(Instant::now()));
    REGISTRY.with(|reg| {
        reg.conns.borrow_mut().push(conn);
        let want = (finest / 4).clamp(MIN_TICK, MAX_TICK);
        if want < reg.tick.get() {
            reg.tick.set(want);
        }
        if !reg.running.get() {
            reg.running.set(true);
            rt_core::spawn(sweep(Rc::clone(reg)));
        }
    });
}

async fn sweep(reg: Rc<Registry>) {
    loop {
        rt_core::sleep(reg.tick.get()).await;
        let now = Instant::now();
        NOW.with(|n| n.set(now));
        // Take the list out so a `beat` may register/close freely.
        let mut conns = std::mem::take(&mut *reg.conns.borrow_mut());
        conns.retain(|w| w.upgrade().is_some_and(|c| c.beat(now)));
        // Connections registered during the walk were pushed to the (now empty)
        // live list; keep them too.
        let mut live = reg.conns.borrow_mut();
        conns.append(&mut live);
        *live = conns;
        if live.is_empty() {
            reg.running.set(false);
            reg.tick.set(MAX_TICK);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter {
        beats: Cell<u32>,
        stop_after: u32,
    }

    impl Beat for Counter {
        fn beat(&self, _now: Instant) -> bool {
            self.beats.set(self.beats.get() + 1);
            self.beats.get() < self.stop_after
        }
    }

    #[test]
    fn one_sweeper_serves_many_connections_and_stops_when_idle() {
        let ex = rt_core::Executor::new().unwrap();
        let conns: Vec<Rc<Counter>> = (0..1000)
            .map(|i| {
                Rc::new(Counter {
                    beats: Cell::new(0),
                    stop_after: 1 + i % 3,
                })
            })
            .collect();
        ex.block_on(async {
            for c in &conns {
                let weak: Weak<dyn Beat> = Rc::downgrade(&(Rc::clone(c) as Rc<dyn Beat>));
                register(weak, Duration::from_millis(40));
            }
            rt_core::sleep(Duration::from_millis(120)).await;
        })
        .unwrap();
        for (i, c) in conns.iter().enumerate() {
            assert_eq!(c.beats.get(), 1 + i as u32 % 3, "conn {i}");
        }
        REGISTRY.with(|r| assert!(!r.running.get() && r.conns.borrow().is_empty()));
    }
}
