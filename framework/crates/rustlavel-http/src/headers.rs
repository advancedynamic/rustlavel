//! Case-insensitive header storage.
//!
//! Names are lowercased on insert, which is how HTTP/2 sends them anyway, and
//! makes lookups a plain map hit rather than a scan.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: BTreeMap<String, Vec<String>>,
}

/// Take the line terminators out of a header name or value.
///
/// A carriage return or newline in a header value ends the header — and, if
/// there are two, the whole head. So a value an attacker reaches turns into
/// headers of their choosing and then a body of their choosing: a `Set-Cookie`
/// that fixes a session, or a second response the cache in front will serve to
/// somebody else. Response splitting, and the shape it usually arrives in is
/// ordinary: `Response::see_other(req.input("next")...)`, where `next` came
/// from a form and `%0d%0a` survived being decoded.
///
/// Removed rather than refused, because `set` has no way to report a refusal
/// and an API change here would reach every call site in the framework. A
/// header value with a newline in it is a bug or an attack in every case, so
/// there is nothing worth preserving in the part that is dropped.
///
/// The other C0 controls go too: they have no meaning in a header, and a NUL
/// is read differently by different proxies, which is the same class of
/// disagreement that makes request smuggling work.
fn sanitise(text: &str) -> String {
    text.chars().filter(|c| *c == '\t' || !c.is_control()).collect()
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace any existing values for this name.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.entries.insert(sanitise(name).to_ascii_lowercase(), vec![sanitise(&value.into())]);
    }

    /// Add a value, keeping the ones already present. Used for `Set-Cookie`,
    /// which is the one header that legitimately repeats.
    pub fn append(&mut self, name: &str, value: impl Into<String>) {
        self.entries
            .entry(sanitise(name).to_ascii_lowercase())
            .or_default()
            .push(sanitise(&value.into()));
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

    /// Response splitting: a newline in a header value ends the header, and two
    /// end the whole head — so a value an attacker reaches becomes headers of
    /// their choosing and then a body of their choosing. The shape it arrives
    /// in is ordinary: a redirect built from a form field where `%0d%0a`
    /// survived being decoded.
    #[test]
    fn a_newline_cannot_be_smuggled_into_a_header() {
        let mut headers = Headers::new();
        headers.set("location", "/next\r\nset-cookie: session=attacker\r\n\r\n<html>");

        let value = headers.get("location").expect("the header is still set");
        assert!(!value.contains('\r') && !value.contains('\n'), "{value:?}");
        assert!(!value.contains("\r\nset-cookie"), "a second header was smuggled: {value:?}");
        assert_eq!(value, "/nextset-cookie: session=attacker<html>");
    }

    #[test]
    fn append_is_guarded_too_and_so_is_the_name() {
        let mut headers = Headers::new();
        headers.append("set-cookie", "a=1\nx-evil: yes");
        headers.set("x-name\r\ninjected", "fine");

        assert!(!headers.get("set-cookie").unwrap().contains('\n'));
        assert!(headers.get("x-nameinjected").is_some(), "the name was not cleaned");
    }

    /// A tab is legal inside a header value, and folding used to rely on it.
    /// Stripping it would corrupt values nobody was attacking.
    #[test]
    fn a_tab_survives() {
        let mut headers = Headers::new();
        headers.set("x-thing", "a\tb");
        assert_eq!(headers.get("x-thing"), Some("a\tb"));
    }
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
