//! Scrubbing credentials out of anything Telescope is about to remember.
//!
//! The dashboard shows raw SQL, log lines and whatever fields a package chose
//! to emit, and those pages get screenshotted, pasted into issues, and left
//! open on a projector. Redaction happens once, at record time, rather than at
//! render time: an entry that never held the secret cannot leak it through the
//! JSON API, through the on-disk journal, or through a future exporter nobody
//! has written yet.

use rustlavel_core::Json;
use std::collections::BTreeMap;

/// What a redacted value is replaced with. Deliberately visible, so a developer
/// can tell "we hid this" apart from "this was empty".
pub const REDACTED: &str = "[redacted]";

/// Key fragments that mark a value as a credential.
///
/// Matched as substrings of the lowercased key, so `db_password`,
/// `X-Api-Key` and `refreshToken` are all caught without an exhaustive list.
const SENSITIVE: &[&str] = &[
    "password",
    "passwd",
    "token",
    "secret",
    "authorization",
    "api_key",
    "apikey",
    "api-key",
    "credential",
    "private_key",
];

/// Whether a field name looks like it holds a credential.
pub fn is_sensitive(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    SENSITIVE.iter().any(|marker| lowered.contains(marker))
}

/// Redact a whole field map, descending into nested objects and arrays.
pub fn fields(fields: BTreeMap<String, Json>) -> BTreeMap<String, Json> {
    fields.into_iter().map(|(key, value)| (key.clone(), value_for(&key, value))).collect()
}

/// Redact one value, given the key it was found under.
///
/// A sensitive key hides the whole subtree: `{"auth": {"token": "x"}}` under a
/// key named `authorization` should not survive because its leaves happen to be
/// named innocently.
fn value_for(key: &str, value: Json) -> Json {
    if is_sensitive(key) {
        return Json::String(REDACTED.to_string());
    }
    match value {
        // An array inherits its parent's key: `tokens: ["a", "b"]` is already
        // handled above, so this only recurses into objects inside the array.
        Json::Array(items) => Json::Array(items.into_iter().map(|item| value_for(key, item)).collect()),
        Json::Object(map) => {
            Json::Object(map.into_iter().map(|(k, v)| (k.clone(), value_for(&k, v))).collect())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_that_look_like_credentials_are_recognised() {
        assert!(is_sensitive("password"));
        assert!(is_sensitive("DB_PASSWORD"));
        assert!(is_sensitive("Authorization"));
        assert!(is_sensitive("refreshToken"));
        assert!(is_sensitive("x-api-key"));
        assert!(!is_sensitive("path"));
        assert!(!is_sensitive("status"));
    }

    #[test]
    fn sensitive_values_are_replaced_but_their_neighbours_survive() {
        let redacted = fields(BTreeMap::from([
            ("path".to_string(), Json::from("/login")),
            ("password".to_string(), Json::from("hunter2")),
        ]));

        assert_eq!(redacted["path"].as_str(), Some("/login"));
        assert_eq!(redacted["password"].as_str(), Some(REDACTED));
    }

    #[test]
    fn nested_objects_are_scrubbed_too() {
        let nested = Json::object([
            ("user", Json::from("ada")),
            ("api_key", Json::from("sk-live-123")),
        ]);
        let redacted = fields(BTreeMap::from([("payload".to_string(), nested)]));

        let rendered = redacted["payload"].to_string();
        assert!(!rendered.contains("sk-live-123"));
        assert!(rendered.contains("ada"));
    }

    #[test]
    fn a_sensitive_key_hides_its_entire_subtree() {
        let redacted = fields(BTreeMap::from([(
            "authorization".to_string(),
            Json::object([("scheme", Json::from("Bearer")), ("value", Json::from("abc"))]),
        )]));

        assert_eq!(redacted["authorization"].as_str(), Some(REDACTED));
    }
}
