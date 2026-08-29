//! Our own `.env` loader.
//!
//! Supports `KEY=value`, `export KEY=value`, `#` comments, single/double quoted
//! values (with escapes inside double quotes), and `${VAR}` interpolation
//! against values already loaded or present in the process environment.

use crate::error::{Error, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Parse `.env` text into ordered key/value pairs.
///
/// Interpolation resolves against earlier entries in the same file first, then
/// the process environment — the same precedence Laravel's loader uses.
pub fn parse(source: &str, file: &str) -> Result<BTreeMap<String, String>> {
    let mut values: BTreeMap<String, String> = BTreeMap::new();

    for (index, raw_line) in source.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            return Err(Error::Config {
                file: file.to_string(),
                line: line_number,
                message: format!("expected `KEY=value`, found `{line}`"),
            });
        };

        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            return Err(Error::Config {
                file: file.to_string(),
                line: line_number,
                message: format!("`{key}` is not a valid variable name"),
            });
        }

        let value = parse_value(value.trim(), &values, file, line_number)?;
        values.insert(key.to_string(), value);
    }

    Ok(values)
}

fn parse_value(
    raw: &str,
    known: &BTreeMap<String, String>,
    file: &str,
    line: usize,
) -> Result<String> {
    let mut chars = raw.chars().peekable();
    match chars.peek() {
        // Single quotes are literal: no escapes, no interpolation.
        Some('\'') => {
            let body = raw
                .strip_prefix('\'')
                .and_then(|r| r.strip_suffix('\''))
                .ok_or_else(|| Error::Config {
                    file: file.to_string(),
                    line,
                    message: "unterminated single-quoted value".into(),
                })?;
            Ok(body.to_string())
        }
        Some('"') => {
            let body = raw
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .ok_or_else(|| Error::Config {
                    file: file.to_string(),
                    line,
                    message: "unterminated double-quoted value".into(),
                })?;
            let unescaped = unescape(body);
            Ok(interpolate(&unescaped, known))
        }
        _ => {
            // Bare values end at an unquoted `#` comment.
            let body = match raw.find(" #") {
                Some(at) => &raw[..at],
                None => raw,
            };
            Ok(interpolate(body.trim(), known))
        }
    }
}

fn unescape(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('$') => out.push('$'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn interpolate(value: &str, known: &BTreeMap<String, String>) -> String {
    if !value.contains("${") {
        return value.to_string();
    }

    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{')
            && let Some(end) = value[i + 2..].find('}') {
                let name = &value[i + 2..i + 2 + end];
                let resolved = known
                    .get(name)
                    .cloned()
                    .or_else(|| std::env::var(name).ok())
                    .unwrap_or_default();
                out.push_str(&resolved);
                i += end + 3;
                continue;
            }
        // Push whole UTF-8 characters, never partial bytes.
        let ch = value[i..].chars().next().expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Load a `.env` file and export every entry into the process environment.
///
/// Variables already set in the real environment win, so deployments can
/// override the file without editing it. A missing file is not an error.
pub fn load(path: impl AsRef<Path>) -> Result<BTreeMap<String, String>> {
    let path = path.as_ref();
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(Error::Io(e)),
    };

    let values = parse(&source, &path.display().to_string())?;
    for (key, value) in &values {
        if std::env::var_os(key).is_none() {
            // SAFETY: called during single-threaded application boot, before
            // any worker task that might read the environment concurrently.
            unsafe { std::env::set_var(key, value) };
        }
    }
    Ok(values)
}

/// Read an environment variable, falling back to a default.
///
/// This is the `env('APP_NAME', 'Rustlavel')` of the framework.
pub fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_common_shapes() {
        let source = r#"
# a comment
APP_NAME=Rustlavel
export APP_ENV=local
APP_DEBUG=true            # trailing comment
QUOTED="hello world"
LITERAL='raw ${APP_NAME}'
ESCAPED="line\nbreak"
EMPTY=
"#;
        let values = parse(source, ".env").unwrap();

        assert_eq!(values["APP_NAME"], "Rustlavel");
        assert_eq!(values["APP_ENV"], "local");
        assert_eq!(values["APP_DEBUG"], "true");
        assert_eq!(values["QUOTED"], "hello world");
        assert_eq!(values["LITERAL"], "raw ${APP_NAME}");
        assert_eq!(values["ESCAPED"], "line\nbreak");
        assert_eq!(values["EMPTY"], "");
    }

    #[test]
    fn interpolates_earlier_entries() {
        let values = parse("HOST=localhost\nURL=http://${HOST}:8000/app", ".env").unwrap();
        assert_eq!(values["URL"], "http://localhost:8000/app");
    }

    #[test]
    fn unknown_interpolation_becomes_empty() {
        let values = parse("URL=http://${NOPE}/x", ".env").unwrap();
        assert_eq!(values["URL"], "http:///x");
    }

    #[test]
    fn reports_the_offending_line() {
        let err = parse("GOOD=1\nthis line is broken\n", ".env").unwrap_err();
        match err {
            Error::Config { line, .. } => assert_eq!(line, 2),
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_not_an_error() {
        assert!(load("/definitely/not/here/.env").unwrap().is_empty());
    }
}
