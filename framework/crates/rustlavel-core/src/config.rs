//! Configuration: `config.get("app.name")`.
//!
//! Values are held as [`Json`] so a config tree can be built in Rust, loaded
//! from a `.json` file under `config/`, or overridden from the environment —
//! all through one lookup path.

use crate::env;
use crate::error::Result;
use crate::json::Json;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

/// The application's configuration tree.
///
/// Cloning is cheap: every clone shares one store, so a handler can hold a
/// `Config` without copying the tree.
#[derive(Clone, Default)]
pub struct Config {
    values: Arc<RwLock<BTreeMap<String, Json>>>,
}

impl Config {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the tree with the framework's defaults, honouring `.env`.
    pub fn with_defaults() -> Self {
        let config = Config::new();
        config.set(
            "app",
            Json::object([
                ("name", Json::from(env::env_or("APP_NAME", "Rustlavel"))),
                ("env", Json::from(env::env_or("APP_ENV", "local"))),
                ("debug", Json::from(env::env_or("APP_DEBUG", "true") == "true")),
                ("url", Json::from(env::env_or("APP_URL", "http://localhost:8000"))),
                ("key", Json::from(env::env_or("APP_KEY", ""))),
            ]),
        );
        config.set(
            "server",
            Json::object([
                ("host", Json::from(env::env_or("SERVER_HOST", "127.0.0.1"))),
                ("port", Json::from(env::env_or("SERVER_PORT", "8000").parse::<u16>().unwrap_or(8000))),
            ]),
        );
        config
    }

    /// Load every `config/*.json` file, keyed by file stem.
    ///
    /// `config/app.json` becomes the `app.*` namespace, mirroring Laravel's
    /// `config/app.php`. String values support `${VAR}` interpolation so a
    /// config file can defer to `.env`.
    pub fn load_dir(&self, dir: impl AsRef<Path>) -> Result<()> {
        let dir = dir.as_ref();
        if !dir.is_dir() {
            return Ok(());
        }

        let mut entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        entries.sort();

        for path in entries {
            let Some(namespace) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let source = std::fs::read_to_string(&path)?;
            let parsed = Json::parse(&source)?;
            self.merge(namespace, expand_env(parsed));
        }
        Ok(())
    }

    /// Set (or replace) a value at a dotted path.
    pub fn set(&self, path: &str, value: impl Into<Json>) {
        let value = value.into();
        let mut values = self.values.write().expect("config lock poisoned");
        match path.split_once('.') {
            None => {
                values.insert(path.to_string(), value);
            }
            Some((root, rest)) => {
                let entry = values.entry(root.to_string()).or_insert_with(|| Json::Object(BTreeMap::new()));
                set_nested(entry, rest, value);
            }
        }
    }

    /// Merge an object into a namespace, keeping keys that are not overridden.
    pub fn merge(&self, namespace: &str, value: Json) {
        let mut values = self.values.write().expect("config lock poisoned");
        match values.get_mut(namespace) {
            Some(existing) => merge_into(existing, value),
            None => {
                values.insert(namespace.to_string(), value);
            }
        }
    }

    /// Look up a dotted path: `config.get("app.name")`.
    pub fn get(&self, path: &str) -> Option<Json> {
        let values = self.values.read().expect("config lock poisoned");
        match path.split_once('.') {
            None => values.get(path).cloned(),
            Some((root, rest)) => values.get(root)?.get(rest).cloned(),
        }
    }

    pub fn string(&self, path: &str, default: &str) -> String {
        self.get(path)
            .and_then(|v| match v {
                Json::String(s) => Some(s),
                Json::Number(n) => Some(n.to_string()),
                Json::Bool(b) => Some(b.to_string()),
                _ => None,
            })
            .unwrap_or_else(|| default.to_string())
    }

    pub fn int(&self, path: &str, default: i64) -> i64 {
        self.get(path)
            .and_then(|v| match v {
                Json::Number(n) => Some(n as i64),
                Json::String(s) => s.parse().ok(),
                _ => None,
            })
            .unwrap_or(default)
    }

    pub fn bool(&self, path: &str, default: bool) -> bool {
        self.get(path)
            .and_then(|v| match v {
                Json::Bool(b) => Some(b),
                Json::String(s) => match s.as_str() {
                    "true" | "1" | "yes" | "on" => Some(true),
                    "false" | "0" | "no" | "off" => Some(false),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or(default)
    }

    /// The current environment name: `local`, `production`, `testing`.
    pub fn environment(&self) -> String {
        self.string("app.env", "local")
    }

    pub fn is_local(&self) -> bool {
        self.environment() == "local"
    }

    pub fn is_production(&self) -> bool {
        self.environment() == "production"
    }

    /// Whether to show the detailed error page. Never true in production.
    pub fn debug(&self) -> bool {
        self.bool("app.debug", true) && !self.is_production()
    }
}

fn set_nested(target: &mut Json, path: &str, value: Json) {
    if !matches!(target, Json::Object(_)) {
        *target = Json::Object(BTreeMap::new());
    }
    let Json::Object(map) = target else { unreachable!() };

    match path.split_once('.') {
        None => {
            map.insert(path.to_string(), value);
        }
        Some((head, rest)) => {
            let entry = map.entry(head.to_string()).or_insert_with(|| Json::Object(BTreeMap::new()));
            set_nested(entry, rest, value);
        }
    }
}

fn merge_into(target: &mut Json, incoming: Json) {
    match (target, incoming) {
        (Json::Object(existing), Json::Object(new)) => {
            for (key, value) in new {
                match existing.get_mut(&key) {
                    Some(slot) => merge_into(slot, value),
                    None => {
                        existing.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

/// Replace `${VAR}` inside every string of a loaded config document.
fn expand_env(value: Json) -> Json {
    match value {
        Json::String(s) if s.contains("${") => Json::String(expand_str(&s)),
        Json::Array(items) => Json::Array(items.into_iter().map(expand_env).collect()),
        Json::Object(map) => Json::Object(map.into_iter().map(|(k, v)| (k, expand_env(v))).collect()),
        other => other,
    }
}

fn expand_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let (name, default) = match after[..end].split_once(':') {
                    Some((name, default)) => (name, default),
                    None => (&after[..end], ""),
                };
                out.push_str(&env::env_or(name.trim(), default));
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_and_reads_nested_paths() {
        let config = Config::new();
        config.set("app.name", "Rustlavel");
        config.set("app.nested.deep", 7);

        assert_eq!(config.string("app.name", ""), "Rustlavel");
        assert_eq!(config.int("app.nested.deep", 0), 7);
        assert_eq!(config.string("app.missing", "fallback"), "fallback");
    }

    #[test]
    fn merge_keeps_untouched_keys() {
        let config = Config::new();
        config.set("app", Json::object([("name", "Old".into()), ("env", "local".into())]));
        config.merge("app", Json::object([("name", "New".into())]));

        assert_eq!(config.string("app.name", ""), "New");
        assert_eq!(config.string("app.env", ""), "local");
    }

    #[test]
    fn debug_is_forced_off_in_production() {
        let config = Config::new();
        config.set("app.debug", true);
        config.set("app.env", "production");
        assert!(!config.debug());
    }

    #[test]
    fn config_strings_expand_environment_variables() {
        // SAFETY: single-threaded test setup.
        unsafe { std::env::set_var("RUSTLAVEL_TEST_HOST", "db.internal") };
        let expanded = expand_env(Json::from("${RUSTLAVEL_TEST_HOST}:5432"));
        assert_eq!(expanded.as_str(), Some("db.internal:5432"));

        let with_default = expand_env(Json::from("${RUSTLAVEL_TEST_ABSENT:fallback}"));
        assert_eq!(with_default.as_str(), Some("fallback"));
    }
}
