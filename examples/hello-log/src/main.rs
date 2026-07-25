//! hello-log — see kernway-log's output, levels, per-module filters, and formats.
//!
//! ```text
//! cargo run -p hello-log                                   # Info, Pretty
//! KW_LOG=debug cargo run -p hello-log                      # everything at Debug
//! KW_LOG=info,kernway_redis=trace cargo run -p hello-log   # per-module: only redis is louder
//! KW_LOG_FORMAT=json cargo run -p hello-log                # line-delimited JSON
//! ```
//!
//! `KW_LOG` is the filter (a bare level is the default; `target=level` overrides a
//! module). `KW_LOG_FORMAT=json` switches the format. The `target:` on a macro
//! stands in for a framework module so one process can show the filter at work.

use kernway_log::{Filter, Format, Logger};

fn main() {
    let format = match std::env::var("KW_LOG_FORMAT").as_deref() {
        Ok("json") => Format::Json,
        _ => Format::Pretty,
    };
    // Filter from KW_LOG (default Info). Install the process logger.
    kernway_log::init(Logger::new(Filter::from_env(), format));

    // A handful of records, as different parts of a framework would emit them.
    kernway_log::info!("kernway starting on {}", "0.0.0.0:8080");
    kernway_log::info!(target: "kernway_server", "4 routes registered");
    kernway_log::debug!(target: "kernway_security", "authenticated user {}", "alice");
    kernway_log::debug!(target: "kernway_redis", "GET session:8f14e45f -> hit");
    kernway_log::trace!(target: "kernway_redis", "RESP: *2 $3 GET ...");
    kernway_log::warn!(target: "kernway_security", "login rate limit near for {}", "1.2.3.4");
    kernway_log::error!(target: "kernway_security", "login failed: {}", "session store unavailable");
    kernway_log::info!(target: "kernway_server", "GET /protected 200 3ms");
}
