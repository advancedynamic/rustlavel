//! Case-insensitive header storage.
//!
//! Names are lowercased on insert, which is how HTTP/2 sends them anyway, and
//! makes lookups a plain map hit rather than a scan.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: BTreeMap<String, Vec<String>>,
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace any existing values for this name.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.entries.insert(name.to_ascii_lowercase(), vec![value.into()]);
    }

    /// Add a value, keeping the ones already present. Used for `Set-Cookie`,
    /// which is the one header that legitimately repeats.
    pub fn append(&mut self, name: &str, value: impl Into<String>) {
        self.entries.entry(name.to_ascii_lowercase()).or_default().push(value.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries.get(&name.to_ascii_lowercase()).and_then(|v| v.first()).map(String::as_str)
    }

    pub fn get_all(&self, name: &str) -> &[String] {
        self.entries.get(&name.to_ascii_lowercase()).map_or(&[], Vec::as_slice)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(&name.to_ascii_lowercase())
    }

    pub fn remove(&mut self, name: &str) {
        self.entries.remove(&name.to_ascii_lowercase());
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate every (name, value) pair, repeated headers included.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .flat_map(|(name, values)| values.iter().map(move |value| (name.as_str(), value.as_str())))
    }

    /// The parsed `Content-Length`, when present and well-formed.
    pub fn content_length(&self) -> Option<usize> {
        self.get("content-length")?.trim().parse().ok()
    }

    /// The media type without parameters: `application/json; charset=utf-8` → `application/json`.
    pub fn content_type(&self) -> Option<&str> {
        let value = self.get("content-type")?;
        Some(value.split(';').next().unwrap_or(value).trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_ignore_case() {
        let mut headers = Headers::new();
        headers.set("Content-Type", "application/json");

        assert_eq!(headers.get("content-type"), Some("application/json"));
        assert_eq!(headers.get("CONTENT-TYPE"), Some("application/json"));
        assert!(headers.contains("Content-Type"));
    }

    #[test]
    fn set_replaces_while_append_accumulates() {
        let mut headers = Headers::new();
        headers.set("x-tag", "one");
        headers.set("x-tag", "two");
        assert_eq!(headers.get_all("x-tag"), ["two"]);

        headers.append("set-cookie", "a=1");
        headers.append("set-cookie", "b=2");
        assert_eq!(headers.get_all("set-cookie").len(), 2);
    }

    #[test]
    fn parses_content_metadata() {
        let mut headers = Headers::new();
        headers.set("content-type", "application/json; charset=utf-8");
        headers.set("content-length", "42");

        assert_eq!(headers.content_type(), Some("application/json"));
        assert_eq!(headers.content_length(), Some(42));
    }
}
