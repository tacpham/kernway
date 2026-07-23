//! Interrupt signals — the outside world asking the server to stop.
//!
//! Per the platform rule this is the only place besides its siblings where
//! `#[cfg(…)]` appears in the crate.

use std::io;

#[cfg(unix)]
mod imp {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Once;
    use std::time::Duration;

    /// Set by the signal handler, read by the watcher thread.
    static INTERRUPTED: AtomicBool = AtomicBool::new(false);
    static INSTALLED: Once = Once::new();

    /// How long the watcher may sleep before noticing the flag. Shutdown is a
    /// human-scale event; 50ms is imperceptible and costs 20 wakeups a second on
    /// one thread that is otherwise idle.
    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    /// Runs in signal context, so it may only do async-signal-safe work — a
    /// single relaxed store. Everything else (waking shards, draining
    /// connections) happens on the watcher thread.
    extern "C" fn on_signal(_signum: libc::c_int) {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }

    fn install(signum: libc::c_int) -> io::Result<()> {
        // SAFETY: `on_signal` has the C signature `sighandler_t` requires and
        // touches nothing but one atomic, which is async-signal-safe.
        let handler = on_signal as *const () as libc::sighandler_t;
        let previous = unsafe { libc::signal(signum, handler) };
        if previous == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Restore the default disposition, so a *second* Ctrl+C kills the process
    /// outright. A drain that turns out to be stuck must still be escapable.
    fn restore(signum: libc::c_int) {
        // SAFETY: `SIG_DFL` is always a valid disposition.
        unsafe { libc::signal(signum, libc::SIG_DFL) };
    }

    pub(super) fn on_interrupt(callback: impl FnOnce() + Send + 'static) -> io::Result<()> {
        let mut result = Ok(());
        INSTALLED.call_once(|| {
            result = install(libc::SIGINT).and_then(|()| install(libc::SIGTERM));
        });
        result?;

        std::thread::Builder::new()
            .name("kernway-signal".into())
            .spawn(move || {
                while !INTERRUPTED.load(Ordering::Relaxed) {
                    std::thread::sleep(POLL_INTERVAL);
                }
                restore(libc::SIGINT);
                restore(libc::SIGTERM);
                callback();
            })?;
        Ok(())
    }

    /// Pretend a signal arrived — the only way to exercise the watcher without
    /// actually signalling the test runner's own process group.
    #[cfg(test)]
    pub(super) fn raise_for_test() {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }
}

#[cfg(not(unix))]
mod imp {
    use std::io;

    pub(super) fn on_interrupt(_callback: impl FnOnce() + Send + 'static) -> io::Result<()> {
        // Windows wants `SetConsoleCtrlHandler`, which is not written yet. Say
        // so rather than returning `Ok` and silently never firing.
        Err(super::super::unsupported(
            "interrupt handling is not implemented on this platform",
        ))
    }
}

/// Call `callback` once, on a dedicated thread, when the process is asked to
/// stop (`SIGINT` from Ctrl+C, or `SIGTERM` from an orchestrator).
///
/// The callback runs on an ordinary thread, not in signal context, so it may do
/// anything — typically [`Shutdown::trigger`](crate::Shutdown::trigger).
///
/// A second interrupt after the first is handled by the *default* disposition
/// and kills the process, which is what an operator expects when a graceful
/// drain is taking too long.
///
/// # Portability
/// Unix only. Elsewhere this returns [`io::ErrorKind::Unsupported`] — treat it
/// the way [`pin_current_thread_to_core`](super::pin_current_thread_to_core) is
/// treated: log it and run without the feature.
pub fn on_interrupt(callback: impl FnOnce() + Send + 'static) -> io::Result<()> {
    imp::on_interrupt(callback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    #[cfg(unix)]
    fn the_callback_runs_when_an_interrupt_arrives() {
        let (tx, rx) = mpsc::channel();
        on_interrupt(move || {
            let _ = tx.send(());
        })
        .unwrap();
        imp::raise_for_test();
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the watcher never observed the interrupt");
    }

    #[test]
    #[cfg(not(unix))]
    fn an_unsupported_platform_says_so() {
        let err = on_interrupt(|| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
