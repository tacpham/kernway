//! # kernway-config
//!
//! Layered configuration (KEP-0007): a committed `application.properties` of
//! defaults, an optional `application-{profile}.properties` for the active profile,
//! and `KW_`-prefixed environment overrides — each layer overriding the last, read
//! through a typed [`Config`]. The `.properties` format (Spring's, dotted `key =
//! value`) is parsed here, no `toml`/`serde`/`yaml` dependency for a startup-time,
//! few-lines job (KEP-0000 §1).
//!
//! ```
//! use kernway_config::Config;
//!
//! let config = Config::builder()
//!     .parse("server.port = 8080\nlogging.level.kernway_redis = debug")
//!     .set("server.port", "9090") // a later layer wins
//!     .build();
//!
//! assert_eq!(config.get_or("server.port", 0u16), 9090);
//! assert_eq!(config.get_str("logging.level.kernway_redis"), Some("debug"));
//! ```
//!
//! The standard load — base file, profile file, then env — is [`Config::load`].

#![forbid(unsafe_code)]

// Let `#[configuration]`-generated `::kernway_config::…` paths resolve inside this
// crate's own tests (the same self-alias serde uses for its derive).
extern crate self as kernway_config;

use std::collections::HashMap;
use std::str::FromStr;

/// The `KW_` prefix that marks an environment variable as configuration.
const ENV_PREFIX: &str = "KW_";

/// A typed view over a section of [`Config`], built by `#[configuration]`.
///
/// The derive reads each field from `{prefix}.{field-name}` (underscores become
/// hyphens): an `Option<T>` field is left `None` when the key is absent, any other
/// field falls back to `Default`. Register the result as a DI bean and inject it
/// like any other.
pub trait FromConfig: Sized {
    /// Build this configuration from `config`.
    fn from_config(config: &Config) -> Self;
}

/// Resolved configuration: a flat map of dotted keys to string values. Values stay
/// strings until [`get`](Config::get) parses them.
#[derive(Debug, Clone, Default)]
pub struct Config {
    map: HashMap<String, String>,
    profile: Option<String>,
}

impl Config {
    /// The standard layered load from the current directory: `application.properties`,
    /// then `application-{profile}.properties` (profile from `KW_PROFILE` or the base
    /// file's `kernway.profiles.active`), then `KW_`-prefixed env vars — each
    /// overriding the last.
    #[must_use]
    pub fn load() -> Config {
        Config::load_from(".")
    }

    /// [`load`](Config::load), but from a given directory (for tests and non-cwd
    /// layouts).
    #[must_use]
    pub fn load_from(dir: &str) -> Config {
        let mut map = HashMap::new();

        // 1. Base files — the committed defaults. With the `yaml` feature, a
        // `.yml` is read first and `.properties` overrides it (the native format
        // wins if both exist; most projects use one).
        #[cfg(feature = "yaml")]
        {
            read_yaml_into(&format!("{dir}/application.yml"), &mut map);
            read_yaml_into(&format!("{dir}/application.yaml"), &mut map);
        }
        read_into(&format!("{dir}/application.properties"), &mut map);

        // Resolve the active profile: env wins over a property in the base file.
        let profile = std::env::var("KW_PROFILE")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| map.get("kernway.profiles.active").cloned());

        // 2. Profile files — override the base for this environment.
        if let Some(profile) = &profile {
            #[cfg(feature = "yaml")]
            {
                read_yaml_into(&format!("{dir}/application-{profile}.yml"), &mut map);
                read_yaml_into(&format!("{dir}/application-{profile}.yaml"), &mut map);
            }
            read_into(&format!("{dir}/application-{profile}.properties"), &mut map);
        }

        // 3. Environment — the top layer.
        map_env(ENV_PREFIX, std::env::vars(), &mut map);

        Config { map, profile }
    }

    /// Start an empty builder, for a custom layer order or tests.
    #[must_use]
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::default()
    }

    /// The raw string value for `key`, if present.
    #[must_use]
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    /// The value for `key`, parsed to `T` (via `FromStr`). `None` if the key is
    /// absent or does not parse.
    #[must_use]
    pub fn get<T: FromStr>(&self, key: &str) -> Option<T> {
        self.get_str(key)?.parse().ok()
    }

    /// The value for `key` parsed to `T`, or `default` if absent or unparseable.
    #[must_use]
    pub fn get_or<T: FromStr>(&self, key: &str, default: T) -> T {
        self.get(key).unwrap_or(default)
    }

    /// Whether `key` is set.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// Every entry whose key starts with `prefix`, as `(suffix, value)` where the
    /// suffix is the key with the prefix removed — sorted by suffix for stable
    /// output. The `logging.level.` case: the suffix is the module.
    #[must_use]
    pub fn with_prefix(&self, prefix: &str) -> Vec<(&str, &str)> {
        let mut out: Vec<(&str, &str)> = self
            .map
            .iter()
            .filter_map(|(k, v)| k.strip_prefix(prefix).map(|suffix| (suffix, v.as_str())))
            .collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out
    }

    /// A sub-configuration reparented at `prefix` — every key under `prefix.` with
    /// that prefix stripped. `config.sub("server")` turns `server.port` into `port`.
    #[must_use]
    pub fn sub(&self, prefix: &str) -> Config {
        let dotted = format!("{prefix}.");
        let map = self
            .map
            .iter()
            .filter_map(|(k, v)| k.strip_prefix(&dotted).map(|s| (s.to_string(), v.clone())))
            .collect();
        Config { map, profile: self.profile.clone() }
    }

    /// The active profile, if one was resolved (`prod`, `dev`, …).
    #[must_use]
    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// How many keys are set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether no keys are set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Builds a [`Config`] by layering sources; each call overrides earlier keys, so
/// call order is precedence (lowest first).
#[derive(Debug, Default)]
pub struct ConfigBuilder {
    map: HashMap<String, String>,
    profile: Option<String>,
}

impl ConfigBuilder {
    /// Set one key (the highest-precedence, explicit override).
    #[must_use]
    pub fn set(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.map.insert(key.into(), value.into());
        self
    }

    /// Parse `.properties` text and layer it in (later keys override earlier).
    #[must_use]
    pub fn parse(mut self, contents: &str) -> Self {
        parse_properties(contents, &mut self.map);
        self
    }

    /// Parse YAML text and layer it in, flattened to dotted keys (feature = `yaml`).
    #[cfg(feature = "yaml")]
    #[must_use]
    pub fn yaml(mut self, contents: &str) -> Self {
        parse_yaml(contents, &mut self.map);
        self
    }

    /// Layer in a `.properties` file if it exists (a missing file is a no-op).
    #[must_use]
    pub fn file(mut self, path: &str) -> Self {
        read_into(path, &mut self.map);
        self
    }

    /// Layer in `KW_`-prefixed environment variables (`__` → `.`).
    #[must_use]
    pub fn env(mut self) -> Self {
        map_env(ENV_PREFIX, std::env::vars(), &mut self.map);
        self
    }

    /// Record the active profile (informational; `file` chooses the profile file).
    #[must_use]
    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    /// Finish.
    #[must_use]
    pub fn build(self) -> Config {
        Config { map: self.map, profile: self.profile }
    }
}

/// Parse `.properties` text into `map`: `key = value` per line, `#`/`!` comments and
/// blank lines skipped, the first `=` splits (values may contain `=`), key/value
/// trimmed. Later keys override earlier ones.
fn parse_properties(contents: &str, map: &mut HashMap<String, String>) {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
}

/// Read a properties file into `map` if it exists; a missing/unreadable file is a
/// no-op (that layer is simply absent).
fn read_into(path: &str, map: &mut HashMap<String, String>) {
    if let Ok(contents) = std::fs::read_to_string(path) {
        parse_properties(&contents, map);
    }
}

// --- YAML (feature = "yaml") -----------------------------------------------
//
// YAML is flattened into the *same* dotted-key map, so everything downstream —
// `get`, `with_prefix`, `#[configuration]`, the logging bridge — is unchanged. A
// nested map becomes `a.b.c`; a sequence becomes indexed keys (`a.0`, `a.1`), the
// documented limit until Config grows a real list type.

/// Parse YAML text and flatten every document into `map`.
#[cfg(feature = "yaml")]
fn parse_yaml(contents: &str, map: &mut HashMap<String, String>) {
    if let Ok(docs) = yaml_rust2::YamlLoader::load_from_str(contents) {
        for doc in &docs {
            flatten_yaml(doc, "", map);
        }
    }
}

/// Walk a YAML node, writing scalars at their dotted path.
#[cfg(feature = "yaml")]
fn flatten_yaml(node: &yaml_rust2::Yaml, prefix: &str, map: &mut HashMap<String, String>) {
    use yaml_rust2::Yaml;
    let joined = |key: &str| if prefix.is_empty() { key.to_string() } else { format!("{prefix}.{key}") };
    match node {
        Yaml::Hash(hash) => {
            for (key, value) in hash {
                if let Some(key) = yaml_scalar(key) {
                    flatten_yaml(value, &joined(&key), map);
                }
            }
        }
        Yaml::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                flatten_yaml(value, &joined(&index.to_string()), map);
            }
        }
        // A scalar at the leaf: record it. Null is treated as absent.
        scalar => {
            if let Some(value) = yaml_scalar(scalar) {
                map.insert(prefix.to_string(), value);
            }
        }
    }
}

/// A YAML scalar as a string (`None` for null, a container, or a bad value).
#[cfg(feature = "yaml")]
fn yaml_scalar(node: &yaml_rust2::Yaml) -> Option<String> {
    use yaml_rust2::Yaml;
    match node {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Real(r) => Some(r.clone()),
        Yaml::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Read a YAML file into `map` if it exists (missing file = no-op).
#[cfg(feature = "yaml")]
fn read_yaml_into(path: &str, map: &mut HashMap<String, String>) {
    if let Ok(contents) = std::fs::read_to_string(path) {
        parse_yaml(&contents, map);
    }
}

/// Map `PREFIX`-prefixed env vars into `map`: strip the prefix, lowercase, and turn
/// `__` into `.` (a single `_` stays, since keys contain them). Pure over the var
/// iterator, so it is testable without touching the process environment.
fn map_env(prefix: &str, vars: impl Iterator<Item = (String, String)>, map: &mut HashMap<String, String>) {
    for (name, value) in vars {
        if let Some(rest) = name.strip_prefix(prefix) {
            let key = rest.to_ascii_lowercase().replace("__", ".");
            map.insert(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys_comments_and_blank_lines() {
        let c = Config::builder()
            .parse(
                "# a comment\n\
                 ! also a comment\n\
                 \n\
                 server.port = 8080\n\
                 server.host=0.0.0.0\n\
                 note = a = b\n",
            )
            .build();
        assert_eq!(c.get_str("server.port"), Some("8080"));
        assert_eq!(c.get_str("server.host"), Some("0.0.0.0"));
        assert_eq!(c.get_str("note"), Some("a = b"), "only the first = splits");
        assert_eq!(c.len(), 3, "comments and blanks are not keys");
    }

    #[test]
    fn typed_get_and_defaults() {
        let c = Config::builder().parse("port = 9090\ndebug = true").build();
        assert_eq!(c.get::<u16>("port"), Some(9090));
        assert_eq!(c.get::<bool>("debug"), Some(true));
        assert_eq!(c.get::<u16>("missing"), None);
        assert_eq!(c.get_or("missing", 8080u16), 8080);
        assert_eq!(c.get_or("port", 0u16), 9090);
        assert_eq!(c.get::<u16>("debug"), None, "unparseable → None");
    }

    #[test]
    fn a_later_layer_overrides_an_earlier_one() {
        let c = Config::builder()
            .parse("server.port = 8080")
            .parse("server.port = 8081") // a later source wins
            .set("server.port", "9090") // an explicit set wins over both
            .build();
        assert_eq!(c.get_or("server.port", 0u16), 9090);
    }

    #[test]
    fn with_prefix_returns_sorted_suffixes() {
        let c = Config::builder()
            .parse(
                "logging.level = info\n\
                 logging.level.kernway_redis = debug\n\
                 logging.level.kernway_security = warn\n\
                 server.port = 8080\n",
            )
            .build();
        assert_eq!(
            c.with_prefix("logging.level."),
            vec![("kernway_redis", "debug"), ("kernway_security", "warn")]
        );
    }

    #[test]
    fn sub_reparents_a_section() {
        let c = Config::builder().parse("server.port = 8080\nserver.host = 0.0.0.0\nother = x").build();
        let server = c.sub("server");
        assert_eq!(server.get_str("port"), Some("8080"));
        assert_eq!(server.get_str("host"), Some("0.0.0.0"));
        assert!(!server.contains("other"), "only the server.* subtree");
    }

    use di_macro::configuration;

    #[configuration(prefix = "server")]
    #[derive(Debug, PartialEq)]
    struct ServerConfig {
        port: u16,
        host: String,
        keep_alive_secs: Option<u64>, // → server.keep-alive-secs, optional
    }

    #[test]
    fn configuration_macro_binds_a_section() {
        // All present.
        let c = Config::builder()
            .parse("server.port = 9090\nserver.host = 0.0.0.0\nserver.keep-alive-secs = 30")
            .build();
        assert_eq!(
            ServerConfig::from_config(&c),
            ServerConfig { port: 9090, host: "0.0.0.0".into(), keep_alive_secs: Some(30) }
        );

        // Missing keys: non-Option falls back to Default, Option is None.
        let c2 = Config::builder().parse("server.port = 80").build();
        assert_eq!(
            ServerConfig::from_config(&c2),
            ServerConfig { port: 80, host: String::new(), keep_alive_secs: None }
        );
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn yaml_flattens_into_the_same_dotted_keys() {
        let c = Config::builder()
            .yaml(
                "server:\n  \
                   port: 8080\n  \
                   host: 0.0.0.0\n\
                 logging:\n  \
                   level:\n    \
                     kernway_redis: debug\n\
                 cors:\n  \
                   origins:\n    \
                     - https://a.com\n    \
                     - https://b.com\n",
            )
            .build();
        // Nested map → dotted keys; typed access works as for properties.
        assert_eq!(c.get::<u16>("server.port"), Some(8080));
        assert_eq!(c.get_str("server.host"), Some("0.0.0.0"));
        assert_eq!(c.get_str("logging.level.kernway_redis"), Some("debug"));
        // A sequence → indexed keys (the documented list limitation).
        assert_eq!(c.get_str("cors.origins.0"), Some("https://a.com"));
        assert_eq!(c.get_str("cors.origins.1"), Some("https://b.com"));
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn properties_override_yaml_in_the_same_layer() {
        // Mirrors load_from's order: yaml first, then properties on top.
        let c = Config::builder()
            .yaml("server:\n  port: 8080")
            .parse("server.port = 9090")
            .build();
        assert_eq!(c.get_or("server.port", 0u16), 9090, "properties wins over yaml");
    }

    #[test]
    fn env_binding_maps_prefix_and_double_underscore() {
        let vars = vec![
            ("KW_SERVER__PORT".to_string(), "9090".to_string()),
            ("KW_LOGGING__LEVEL__KERNWAY_REDIS".to_string(), "trace".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()), // not prefixed → ignored
        ];
        let mut map = HashMap::new();
        map_env(ENV_PREFIX, vars.into_iter(), &mut map);
        assert_eq!(map.get("server.port").map(String::as_str), Some("9090"));
        // __ is the separator; the single _ inside the module name stays.
        assert_eq!(map.get("logging.level.kernway_redis").map(String::as_str), Some("trace"));
        assert!(!map.contains_key("path"), "only KW_-prefixed vars are config");
    }
}
