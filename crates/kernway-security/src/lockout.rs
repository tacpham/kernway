//! Login throttling / account lockout — brute-force protection for the login flow.
//!
//! A rate limiter caps *request* volume; this caps *failed logins* for a specific
//! key (a username, an IP, or both), locking it out after too many failures so an
//! attacker cannot grind passwords. It is a building block the login handler drives,
//! not a middleware — only the handler knows whether a credential check passed:
//!
//! ```rust,ignore
//! if !guard.check(&username) {
//!     return locked_response();               // still locked out
//! }
//! if verify_password(pw, &stored) {
//!     guard.record_success(&username);        // clear the failure count
//!     // … issue the session …
//! } else {
//!     guard.record_failure(&username);        // one strike; may now be locked
//!     return unauthorized();
//! }
//! ```
//!
//! Failures are counted within a rolling window, so a few mistakes spread over a day
//! never lock anyone out — only a burst does. A successful login clears the count.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

/// The failure record for one key.
struct Attempts {
    failures: u32,
    window_start: u64,
    locked_until: u64,
}

/// Locks a key out after too many failed logins in a window. `now` is unix seconds;
/// the `*_at` methods take it explicitly (testable), the others read the system clock.
pub struct LoginGuard {
    max_failures: u32,
    lockout_secs: u64,
    window_secs: u64,
    attempts: RwLock<HashMap<String, Attempts>>,
}

impl LoginGuard {
    /// Lock a key for `lockout` after `max_failures` failed attempts within a window
    /// equal to `lockout` (use [`with_window`](Self::with_window) to separate them).
    #[must_use]
    pub fn new(max_failures: u32, lockout: Duration) -> Self {
        let secs = lockout.as_secs();
        Self {
            max_failures: max_failures.max(1),
            lockout_secs: secs,
            window_secs: secs,
            attempts: RwLock::new(HashMap::new()),
        }
    }

    /// Count failures within `window` (rather than defaulting it to the lockout).
    #[must_use]
    pub fn with_window(mut self, window: Duration) -> Self {
        self.window_secs = window.as_secs();
        self
    }

    /// Whether `key` may attempt a login now — `false` while locked out.
    #[must_use]
    pub fn check_at(&self, key: &str, now: u64) -> bool {
        self.attempts
            .read()
            .unwrap()
            .get(key)
            .is_none_or(|a| now >= a.locked_until)
    }

    /// Record a failed login for `key`. Returns `Some(locked_until)` (unix seconds) if
    /// this failure tripped (or is within) a lockout, else `None`.
    pub fn record_failure_at(&self, key: &str, now: u64) -> Option<u64> {
        let mut map = self.attempts.write().unwrap();

        // Occasionally drop entries that are neither locked nor in an active window.
        if map.len() > 1024 {
            map.retain(|_, a| {
                now < a.locked_until || now.saturating_sub(a.window_start) < self.window_secs
            });
        }

        let entry = map.entry(key.to_string()).or_insert(Attempts {
            failures: 0,
            window_start: now,
            locked_until: 0,
        });

        // Start a fresh window if the last one has elapsed.
        if now.saturating_sub(entry.window_start) >= self.window_secs {
            entry.failures = 0;
            entry.window_start = now;
        }
        entry.failures += 1;

        if entry.failures >= self.max_failures {
            entry.locked_until = now + self.lockout_secs;
            entry.failures = 0; // reset so the next window starts clean after the lock
            Some(entry.locked_until)
        } else {
            None
        }
    }

    /// Clear the failure count and any lock for `key` (call on a successful login).
    pub fn record_success(&self, key: &str) {
        self.attempts.write().unwrap().remove(key);
    }

    /// Seconds remaining on `key`'s lockout, or `None` if not locked.
    #[must_use]
    pub fn locked_for_at(&self, key: &str, now: u64) -> Option<u64> {
        self.attempts
            .read()
            .unwrap()
            .get(key)
            .filter(|a| now < a.locked_until)
            .map(|a| a.locked_until - now)
    }

    /// [`check_at`](Self::check_at) using the system clock.
    #[must_use]
    pub fn check(&self, key: &str) -> bool {
        self.check_at(key, now())
    }

    /// [`record_failure_at`](Self::record_failure_at) using the system clock.
    pub fn record_failure(&self, key: &str) -> Option<u64> {
        self.record_failure_at(key, now())
    }

    /// [`locked_for_at`](Self::locked_for_at) using the system clock.
    #[must_use]
    pub fn locked_for(&self, key: &str) -> Option<u64> {
        self.locked_for_at(key, now())
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> LoginGuard {
        // Lock for 15 minutes after 5 failures within 15 minutes.
        LoginGuard::new(5, Duration::from_secs(900))
    }

    #[test]
    fn locks_out_after_the_threshold() {
        let g = guard();
        for n in 1..=4 {
            assert_eq!(
                g.record_failure_at("alice", 1000),
                None,
                "failure {n} not yet locking"
            );
            assert!(g.check_at("alice", 1000), "still allowed after {n}");
        }
        // The 5th failure trips the lock.
        assert_eq!(g.record_failure_at("alice", 1000), Some(1000 + 900));
        assert!(!g.check_at("alice", 1000), "now locked out");
        assert_eq!(g.locked_for_at("alice", 1000), Some(900));
    }

    #[test]
    fn the_lock_expires() {
        let g = guard();
        for _ in 0..5 {
            g.record_failure_at("bob", 1000);
        }
        assert!(!g.check_at("bob", 1000), "locked");
        assert!(
            !g.check_at("bob", 1000 + 899),
            "still locked one second before expiry"
        );
        assert!(g.check_at("bob", 1000 + 900), "unlocked at expiry");
    }

    #[test]
    fn a_success_clears_the_count() {
        let g = guard();
        for _ in 0..4 {
            g.record_failure_at("carol", 1000);
        }
        g.record_success("carol");
        // Back to a clean slate: it takes another full 5 to lock.
        for n in 1..=4 {
            assert_eq!(
                g.record_failure_at("carol", 2000),
                None,
                "failure {n} after the reset"
            );
        }
        assert!(
            g.check_at("carol", 2000),
            "not locked — the earlier failures were cleared"
        );
    }

    #[test]
    fn failures_outside_the_window_do_not_accumulate() {
        let g = guard();
        // Four failures, then a fifth long after the window → the count restarts.
        for _ in 0..4 {
            g.record_failure_at("dave", 1000);
        }
        assert_eq!(
            g.record_failure_at("dave", 1000 + 901),
            None,
            "the window rolled over, so this is failure 1"
        );
        assert!(g.check_at("dave", 1000 + 901), "not locked");
    }

    #[test]
    fn separate_keys_are_independent() {
        let g = guard();
        for _ in 0..5 {
            g.record_failure_at("eve", 1000);
        }
        assert!(!g.check_at("eve", 1000), "eve locked");
        assert!(g.check_at("frank", 1000), "frank unaffected");
    }
}
