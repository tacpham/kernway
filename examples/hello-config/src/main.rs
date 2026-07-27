//! hello-config — layered config (KEP-0007) driving per-module log levels.
//!
//! ```text
//! cargo run -p hello-config
//!   # kernway_redis at debug, kernway_security at warn, everything else info —
//!   # all from application.properties, no recompile.
//!
//! KW_LOGGING__LEVEL__KERNWAY_SECURITY=debug cargo run -p hello-config
//!   # an env override (KW_ prefix, __ -> .) turns security up to debug.
//!
//! KW_SERVER__PORT=9090 cargo run -p hello-config
//!   # overrides server.port from the environment.
//! ```
//!
//! Shows the two halves: [`Config`] resolves file + env into typed values, and the
//! `logging.level.*` keys build a `kernway-log` filter — so a deployment tunes log
//! verbosity per module from config.

use di_macro::configuration;
use kernway_config::{Config, FromConfig};
use kernway_log::{Filter, Format, Logger};

/// A typed view of the `server.*` section — Spring's `@ConfigurationProperties`.
/// `#[configuration]` implements `FromConfig`, reading `server.port`/`server.host`.
#[configuration(prefix = "server")]
struct ServerConfig {
    port: u16,
    host: String,
}

/// Build a `kernway-log` filter from config: `logging.level` is the default, and
/// each `logging.level.<module>` is an override. Reuses the log crate's own filter
/// parser by assembling its `default,module=level,…` spec — no coupling between the
/// two crates, just a few lines at the app layer.
fn log_filter(config: &Config) -> Filter {
    let mut spec = config
        .get_str("logging.level")
        .unwrap_or("info")
        .to_string();
    for (module, level) in config.with_prefix("logging.level.") {
        spec.push(',');
        spec.push_str(module);
        spec.push('=');
        spec.push_str(level);
    }
    Filter::parse(&spec)
}

fn main() {
    // The base defaults are embedded here for a self-contained example; a real app
    // uses `Config::load()`, which reads application.properties from disk, then the
    // profile file, then env. `.env()` layers the KW_ overrides on top.
    let config = Config::builder()
        .parse(include_str!("../application.properties"))
        .env()
        .build();

    // The typed bind: one struct, populated from the server.* keys.
    let server = ServerConfig::from_config(&config);

    println!("--- resolved config ---");
    println!("typed ServerConfig -> bind {}:{}", server.host, server.port);
    println!(
        "server.host = {}",
        config.get_str("server.host").unwrap_or("?")
    );
    println!("server.port = {}", config.get_or("server.port", 0u16));
    println!(
        "logging.level (default) = {}",
        config.get_str("logging.level").unwrap_or("info")
    );
    for (module, level) in config.with_prefix("logging.level.") {
        println!("logging.level.{module} = {level}");
    }
    println!("--- logs (filtered by the config above) ---");

    kernway_log::init(Logger::new(log_filter(&config), Format::Pretty));

    kernway_log::info!(target: "kernway_server", "started on {}:{}",
        config.get_str("server.host").unwrap_or("?"), config.get_or("server.port", 0u16));
    kernway_log::debug!(target: "kernway_redis", "GET session:abc -> hit (redis debug is ON)");
    kernway_log::debug!(target: "kernway_security", "authenticated alice (HIDDEN — security is at warn)");
    kernway_log::warn!(target: "kernway_security", "login rate limit near for 1.2.3.4");
    kernway_log::info!(target: "kernway_web", "GET /health 200 (default info applies)");
}
