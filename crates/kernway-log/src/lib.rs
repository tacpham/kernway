//! # kernway-log
//!
//! A small logging facade — ours, no `tracing`/`log` dependency (KEP-0000 §1). A
//! log line is a **level** check against a **per-module filter**, a **format**
//! (human-readable *Pretty* or line-delimited *JSON*), and a write. That is the
//! whole crate.
//!
//! ```
//! use kernway_log::{Filter, Format, Level, Logger};
//!
//! // Default Info, but Debug for one module and Trace for another —
//! // the shape a `RUST_LOG`-style string parses to.
//! let filter = Filter::parse("info,kernway_security=debug,kernway_redis=trace");
//! kernway_log::init(Logger::new(filter, Format::Pretty));
//!
//! kernway_log::info!("listening on {}", "0.0.0.0:8080");
//! kernway_log::debug!(target: "kernway_security", "authenticated {}", "alice");
//! ```
//!
//! ## Per-module levels
//!
//! Every record has a **target** — by default the `module_path!()` of the call
//! site. The [`Filter`] holds a default level plus target-prefix overrides, and a
//! record is emitted when its level passes the *most specific* matching target.
//! So `kernway_security=debug` turns up the volume for that module and its
//! children without touching anything else — the per-module control a single
//! global level cannot give.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context as TaskCx, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

/// Severity, most severe first. Ordered by verbosity: `Error < Warn < Info <
/// Debug < Trace`, so a record at `level` passes a threshold `t` when `level <= t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Something failed and needs attention.
    Error,
    /// Something is off but the request/operation continued.
    Warn,
    /// The normal, noteworthy events (startup, a login, a request).
    Info,
    /// Detail for diagnosing behaviour.
    Debug,
    /// Very fine-grained tracing.
    Trace,
}

impl Level {
    /// The uppercase name, left-padded to five columns for aligned Pretty output.
    fn padded(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    /// The bare uppercase name (for JSON).
    fn name(self) -> &'static str {
        self.padded().trim_end()
    }

    /// Parse a level name (case-insensitive); `None` if unrecognised.
    fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
}

/// A default level plus per-target overrides. A record passes when its level is at
/// most the level of the longest target prefix that matches (else the default).
#[derive(Debug, Clone)]
pub struct Filter {
    default: Level,
    /// `(target-prefix, level)`, consulted longest-prefix-first.
    targets: Vec<(String, Level)>,
}

impl Filter {
    /// A filter that lets everything at or above `default` through, with no
    /// per-target overrides.
    #[must_use]
    pub fn new(default: Level) -> Self {
        Self {
            default,
            targets: Vec::new(),
        }
    }

    /// Override the level for a target prefix (e.g. `"kernway_redis"`).
    #[must_use]
    pub fn with_target(mut self, prefix: impl Into<String>, level: Level) -> Self {
        self.targets.push((prefix.into(), level));
        // Longest prefix first, so `allows` takes the most specific match.
        self.targets.sort_by_key(|t| std::cmp::Reverse(t.0.len()));
        self
    }

    /// Parse a `RUST_LOG`-style string: a bare level is the default, and
    /// `target=level` entries are overrides. `"info,kernway_redis=trace"`.
    /// Unrecognised entries are ignored.
    #[must_use]
    pub fn parse(spec: &str) -> Self {
        let mut filter = Filter::new(Level::Info);
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match entry.split_once('=') {
                Some((target, level)) => {
                    if let Some(level) = Level::parse(level) {
                        filter = filter.with_target(target.trim(), level);
                    }
                }
                None => {
                    if let Some(level) = Level::parse(entry) {
                        filter.default = level;
                    }
                }
            }
        }
        filter
    }

    /// Read the filter from the `KW_LOG` environment variable, or `Info` if unset.
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var("KW_LOG")
            .map(|s| Filter::parse(&s))
            .unwrap_or_else(|_| Filter::new(Level::Info))
    }

    /// The threshold that applies to `target` — the longest matching prefix, else
    /// the default.
    fn threshold(&self, target: &str) -> Level {
        self.targets
            .iter()
            .find(|(prefix, _)| {
                target == prefix
                    || target.starts_with(&format!("{prefix}::"))
                    || target.starts_with(prefix)
            })
            .map_or(self.default, |(_, level)| *level)
    }

    /// Whether a record at `(target, level)` should be emitted.
    #[must_use]
    pub fn allows(&self, target: &str, level: Level) -> bool {
        level <= self.threshold(target)
    }
}

/// How a record is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Human-readable: `2026-07-25T10:00:00.123Z INFO  target: message`.
    Pretty,
    /// One JSON object per line: `{"ts":…,"level":…,"target":…,"msg":…}`.
    Json,
}

// --- MDC: per-request diagnostic context ----------------------------------
//
// The "who am I logging for" that every line should carry — a request id, maybe a
// user — set once at the start of a request and attached to *every* log emitted
// during it, however deep the call stack, without threading it through by hand
// (Spring's MDC).
//
// The mechanism is a future combinator, not a task-local in the executor: [`scope`]
// wraps a request's future so each *poll* sets a thread-local to that request's
// context and restores the previous value after. On a thread-per-core runtime only
// one task is polled at a time on a thread, so the thread-local always reflects the
// task currently executing — correct even with many requests interleaved on one
// thread, where a plain thread-local would leak one request's id into another's.

/// A set of `key = value` fields carried alongside every log line for the duration
/// of a [`scope`]. Ordered; typically just `("req", id)`, plus a user or tenant.
#[derive(Debug, Clone, Default)]
pub struct Context {
    fields: Vec<(String, String)>,
}

impl Context {
    /// An empty context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a field (e.g. `("req", id)`), returning the context for chaining.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.push((key.into(), value.into()));
        self
    }

    /// Whether the context has no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

thread_local! {
    /// The context active on this thread *right now* — i.e. of the task currently
    /// being polled. `None` outside any [`scope`].
    static CURRENT: RefCell<Option<Arc<Context>>> = const { RefCell::new(None) };
}

/// Run `future` with `context` active: every log emitted while it (or anything it
/// awaits) runs carries the context's fields. Wrap a request's future once, at the
/// top of the chain, and the whole request's logs correlate.
pub fn scope<'a, T>(
    context: Context,
    future: Pin<Box<dyn Future<Output = T> + Send + 'a>>,
) -> Scoped<'a, T> {
    Scoped {
        context: Arc::new(context),
        future,
    }
}

/// The future returned by [`scope`]. Sets the thread-local context around each poll
/// of the wrapped future and restores the prior value after (so scopes nest).
pub struct Scoped<'a, T> {
    context: Arc<Context>,
    future: Pin<Box<dyn Future<Output = T> + Send + 'a>>,
}

impl<T> Future for Scoped<'_, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<T> {
        // `Scoped` is `Unpin` (an `Arc` and a `Pin<Box<…>>`), so no unsafe needed.
        let this = self.as_mut().get_mut();
        let previous =
            CURRENT.with(|current| current.borrow_mut().replace(Arc::clone(&this.context)));
        // Restore on the way out via a guard, so the context is put back even if the
        // inner poll *panics* (unwinds) — otherwise a panicking request would leak its
        // context onto the thread for the next task. Composes with panic isolation.
        let _restore = RestoreContext(Some(previous));
        this.future.as_mut().poll(cx)
    }
}

/// Restores the previous context when dropped — at the end of a poll, or during a
/// panic unwind through it.
struct RestoreContext(Option<Option<Arc<Context>>>);

impl Drop for RestoreContext {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            CURRENT.with(|current| *current.borrow_mut() = previous);
        }
    }
}

/// The context active on this thread now, if any.
fn current() -> Option<Arc<Context>> {
    CURRENT.with(|current| current.borrow().clone())
}

/// The logger: a filter, a format, and a sink. Built once and installed with
/// [`init`], or used directly via [`emit`](Logger::emit).
pub struct Logger {
    filter: Filter,
    format: Format,
    sink: Mutex<Box<dyn Write + Send>>,
}

impl Logger {
    /// A logger writing to stderr. Pair a [`Filter`] with a [`Format`].
    #[must_use]
    pub fn new(filter: Filter, format: Format) -> Self {
        Self {
            filter,
            format,
            sink: Mutex::new(Box::new(std::io::stderr())),
        }
    }

    /// Send the output somewhere other than stderr (a file, a buffer for tests).
    #[must_use]
    pub fn with_sink(mut self, sink: impl Write + Send + 'static) -> Self {
        self.sink = Mutex::new(Box::new(sink));
        self
    }

    /// Emit a record if the filter allows it. Called by the macros; usable
    /// directly too.
    pub fn emit(&self, level: Level, target: &str, args: std::fmt::Arguments) {
        if !self.filter.allows(target, level) {
            return;
        }
        let line = match self.format {
            Format::Pretty => format_pretty(now_millis(), level, target, args),
            Format::Json => format_json(now_millis(), level, target, args),
        };
        // A poisoned sink lock should not take the process down over a log line.
        if let Ok(mut sink) = self.sink.lock() {
            let _ = writeln!(sink, "{line}");
        }
    }
}

impl Default for Logger {
    /// Info level, Pretty, to stderr — sensible until [`init`] is called.
    fn default() -> Self {
        Logger::new(Filter::from_env(), Format::Pretty)
    }
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Install the process logger. The first call wins; later calls are ignored (a
/// logger is global, set once at startup). Returns whether this call installed it.
pub fn init(logger: Logger) -> bool {
    LOGGER.set(logger).is_ok()
}

/// The installed logger, or a default (Info/Pretty/stderr, honouring `KW_LOG`) if
/// [`init`] was never called — so logging works out of the box.
pub fn logger() -> &'static Logger {
    LOGGER.get_or_init(Logger::default)
}

/// Backing call for the macros — do not call directly; use `info!`/`debug!`/… .
#[doc(hidden)]
pub fn __log(level: Level, target: &str, args: std::fmt::Arguments) {
    logger().emit(level, target, args);
}

// --- formatting ------------------------------------------------------------

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn format_pretty(millis: u64, level: Level, target: &str, args: std::fmt::Arguments) -> String {
    format!(
        "{} {} {}{target}: {args}",
        iso8601(millis),
        level.padded(),
        context_pretty()
    )
}

fn format_json(millis: u64, level: Level, target: &str, args: std::fmt::Arguments) -> String {
    format!(
        "{{\"ts\":\"{}\",\"level\":\"{}\",{}\"target\":\"{}\",\"msg\":\"{}\"}}",
        iso8601(millis),
        level.name(),
        context_json(),
        json_escape(target),
        json_escape(&args.to_string()),
    )
}

/// The active context as a `[k=v k=v] ` prefix for Pretty, or empty if none.
fn context_pretty() -> String {
    match current() {
        Some(context) if !context.fields.is_empty() => {
            let pairs: Vec<String> = context
                .fields
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            format!("[{}] ", pairs.join(" "))
        }
        _ => String::new(),
    }
}

/// The active context as `"k":"v",` JSON pairs (inserted before `target`), or empty.
fn context_json() -> String {
    match current() {
        Some(context) if !context.fields.is_empty() => {
            let mut out = String::new();
            for (key, value) in &context.fields {
                out.push_str(&format!(
                    "\"{}\":\"{}\",",
                    json_escape(key),
                    json_escape(value)
                ));
            }
            out
        }
        _ => String::new(),
    }
}

/// UTC ISO-8601 with millis, formatted from unix millis with no date dependency
/// (Howard Hinnant's civil-from-days).
fn iso8601(millis: u64) -> String {
    let secs = millis / 1000;
    let ms = millis % 1000;
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

/// Days since 1970-01-01 → `(year, month, day)`, UTC proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Minimal JSON string escaping for the message/target fields.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// --- macros ----------------------------------------------------------------

/// Log at a given level; `target:` defaults to the call site's `module_path!()`.
#[macro_export]
macro_rules! log {
    ($level:expr, target: $target:expr, $($arg:tt)+) => {
        $crate::__log($level, $target, ::std::format_args!($($arg)+))
    };
    ($level:expr, $($arg:tt)+) => {
        $crate::__log($level, ::std::module_path!(), ::std::format_args!($($arg)+))
    };
}

/// Log at `Error`.
#[macro_export]
macro_rules! error {
    (target: $t:expr, $($a:tt)+) => { $crate::log!($crate::Level::Error, target: $t, $($a)+) };
    ($($a:tt)+) => { $crate::log!($crate::Level::Error, $($a)+) };
}
/// Log at `Warn`.
#[macro_export]
macro_rules! warn {
    (target: $t:expr, $($a:tt)+) => { $crate::log!($crate::Level::Warn, target: $t, $($a)+) };
    ($($a:tt)+) => { $crate::log!($crate::Level::Warn, $($a)+) };
}
/// Log at `Info`.
#[macro_export]
macro_rules! info {
    (target: $t:expr, $($a:tt)+) => { $crate::log!($crate::Level::Info, target: $t, $($a)+) };
    ($($a:tt)+) => { $crate::log!($crate::Level::Info, $($a)+) };
}
/// Log at `Debug`.
#[macro_export]
macro_rules! debug {
    (target: $t:expr, $($a:tt)+) => { $crate::log!($crate::Level::Debug, target: $t, $($a)+) };
    ($($a:tt)+) => { $crate::log!($crate::Level::Debug, $($a)+) };
}
/// Log at `Trace`.
#[macro_export]
macro_rules! trace {
    (target: $t:expr, $($a:tt)+) => { $crate::log!($crate::Level::Trace, target: $t, $($a)+) };
    ($($a:tt)+) => { $crate::log!($crate::Level::Trace, $($a)+) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_ordering_is_by_verbosity() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug < Level::Trace);
    }

    #[test]
    fn a_default_filter_gates_at_its_level() {
        let f = Filter::new(Level::Info);
        assert!(f.allows("anything", Level::Error));
        assert!(f.allows("anything", Level::Info));
        assert!(
            !f.allows("anything", Level::Debug),
            "debug is below the Info threshold"
        );
    }

    #[test]
    fn a_target_override_wins_for_its_module() {
        let f = Filter::new(Level::Info).with_target("kernway_redis", Level::Trace);
        // The override module sees trace…
        assert!(f.allows("kernway_redis", Level::Trace));
        assert!(f.allows("kernway_redis::conn", Level::Debug));
        // …but everything else is still gated at Info.
        assert!(!f.allows("kernway_web", Level::Debug));
    }

    #[test]
    fn the_longest_matching_prefix_applies() {
        let f = Filter::new(Level::Info)
            .with_target("kernway", Level::Warn)
            .with_target("kernway_security", Level::Debug);
        // The more specific prefix wins.
        assert!(f.allows("kernway_security", Level::Debug));
        assert!(!f.allows("kernway_security", Level::Trace));
        // The broader prefix gates its other children at Warn.
        assert!(!f.allows("kernway_web", Level::Info), "kernway* is Warn");
        assert!(f.allows("kernway_web", Level::Warn));
    }

    #[test]
    fn parse_reads_default_and_overrides() {
        let f = Filter::parse("warn, kernway_security=debug ,kernway_redis=trace");
        assert!(!f.allows("other", Level::Info), "default is warn");
        assert!(f.allows("kernway_security", Level::Debug));
        assert!(f.allows("kernway_redis", Level::Trace));
    }

    #[test]
    fn pretty_format_has_the_parts_in_order() {
        // 2021-01-01T00:00:00.000Z
        let line = format_pretty(
            1_609_459_200_000,
            Level::Info,
            "kernway_web",
            format_args!("hi {}", 7),
        );
        assert_eq!(line, "2021-01-01T00:00:00.000Z INFO  kernway_web: hi 7");
    }

    #[test]
    fn json_format_is_one_object_and_escapes() {
        let line = format_json(
            1_609_459_200_000,
            Level::Error,
            "sec",
            format_args!("bad \"quote\"\n"),
        );
        assert_eq!(
            line,
            r#"{"ts":"2021-01-01T00:00:00.000Z","level":"ERROR","target":"sec","msg":"bad \"quote\"\n"}"#
        );
    }

    #[test]
    fn iso8601_is_correct_utc() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_609_459_200_123), "2021-01-01T00:00:00.123Z");
        // A leap-year date, to exercise civil_from_days.
        assert_eq!(iso8601(1_582_934_400_000), "2020-02-29T00:00:00.000Z");
    }

    #[test]
    fn a_logger_writes_only_what_the_filter_allows() {
        use std::sync::{Arc, Mutex};
        // Capture into a shared buffer.
        #[derive(Clone)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let logger = Logger::new(Filter::new(Level::Info), Format::Pretty).with_sink(buf.clone());

        logger.emit(Level::Info, "app", format_args!("shown"));
        logger.emit(Level::Debug, "app", format_args!("hidden"));

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("app: shown"), "info passes: {out}");
        assert!(!out.contains("hidden"), "debug is filtered: {out}");
    }

    #[test]
    fn scope_activates_the_context_during_a_poll_and_restores_after() {
        use std::future::Future;
        assert!(current().is_none(), "no context outside a scope");

        // The wrapped future reads the context that is active while it is polled.
        let fut = scope(
            Context::new().with("req", "abc123"),
            Box::pin(async { current().map(|c| c.fields.clone()) }),
        );
        let mut fut = Box::pin(fut);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let seen = match fut.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => unreachable!("the inner future is ready"),
        };

        assert_eq!(seen, Some(vec![("req".to_string(), "abc123".to_string())]));
        assert!(
            current().is_none(),
            "the context is restored after the scope"
        );
    }

    #[test]
    fn the_formats_carry_the_active_context() {
        use std::sync::Arc;
        CURRENT.with(|c| *c.borrow_mut() = Some(Arc::new(Context::new().with("req", "abc"))));

        assert_eq!(
            format_pretty(1_609_459_200_000, Level::Info, "web", format_args!("hi")),
            "2021-01-01T00:00:00.000Z INFO  [req=abc] web: hi"
        );
        assert_eq!(
            format_json(1_609_459_200_000, Level::Info, "web", format_args!("hi")),
            r#"{"ts":"2021-01-01T00:00:00.000Z","level":"INFO","req":"abc","target":"web","msg":"hi"}"#
        );

        CURRENT.with(|c| *c.borrow_mut() = None); // do not leak into other tests
    }
}
