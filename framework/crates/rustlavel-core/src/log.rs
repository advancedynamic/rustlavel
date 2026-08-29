//! Leveled logging that doubles as instrumentation.
//!
//! Every log line is also dispatched on the event bus, so Telescope and a JSON
//! log shipper see the same records without the caller doing anything twice.

use crate::events::Event;
use crate::json::Json;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Level::Debug => "\x1b[38;5;244m",
            Level::Info => "\x1b[38;5;39m",
            Level::Warn => "\x1b[38;5;214m",
            Level::Error => "\x1b[38;5;203m",
        }
    }

    pub fn parse(value: &str) -> Option<Level> {
        match value.to_ascii_lowercase().as_str() {
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }
}

static MIN_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static USE_JSON: AtomicU8 = AtomicU8::new(0);

/// Drop anything below this level.
pub fn set_level(level: Level) {
    MIN_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Emit one JSON object per line instead of the human format. This is what
/// production deployments want so log aggregators can parse the output.
pub fn set_json(enabled: bool) {
    USE_JSON.store(u8::from(enabled), Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    level as u8 >= MIN_LEVEL.load(Ordering::Relaxed)
}

pub fn log(level: Level, message: impl AsRef<str>) {
    let message = message.as_ref();

    if enabled(level) {
        let mut stderr = std::io::stderr().lock();
        if USE_JSON.load(Ordering::Relaxed) == 1 {
            let line = Json::object([
                ("level", Json::from(level.as_str())),
                ("message", Json::from(message)),
                ("timestamp", Json::from(unix_seconds())),
            ]);
            let _ = writeln!(stderr, "{}", line);
        } else {
            let _ = writeln!(
                stderr,
                "{}{:>5}\x1b[0m  {}",
                level.color(),
                level.as_str().to_uppercase(),
                message
            );
        }
    }

    Event::new("log")
        .with("level", level.as_str())
        .with("message", message)
        .dispatch();
}

fn unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Debug, format!($($arg)*)) };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Info, format!($($arg)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Warn, format!($($arg)*)) };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::log($crate::log::Level::Error, format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_threshold_is_respected() {
        set_level(Level::Warn);
        assert!(!enabled(Level::Info));
        assert!(enabled(Level::Error));
        set_level(Level::Info);
    }

    #[test]
    fn levels_parse_from_configuration_strings() {
        assert_eq!(Level::parse("WARNING"), Some(Level::Warn));
        assert_eq!(Level::parse("nonsense"), None);
    }
}
