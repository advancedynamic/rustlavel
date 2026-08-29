//! What a secret store says when it says no.
//!
//! Vault and OpenBao return the same body for every failure — `{"errors": [...]}`
//! — and put the meaning in the status code. Distinguishing those codes is not
//! pedantry: "the token expired", "this token may not read that path" and "the
//! server is sealed" call for three completely different responses from the
//! caller, and all three are a bare 4xx if you do not look.

use rustlavel_core::{Error, Json};

/// A failure from the secret store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// 400 — the request itself was wrong.
    BadRequest(String),
    /// 403 — no token, an expired one, or one without the policy for this path.
    ///
    /// Vault does not distinguish "expired" from "not allowed", so neither can
    /// this. The message says so rather than guessing.
    PermissionDenied(String),
    /// 404 — no such path, or nothing stored at it.
    ///
    /// Worth its own variant because it is usually *not* an error: reading a
    /// secret that may not be there yet is an ordinary thing to do.
    NotFound { path: String },
    /// 412 — a standby node has not caught up with the value being asked for.
    /// Retrying against the active node is the fix, and it usually works.
    Stale(String),
    /// 429 — a standby that will not serve this request at all.
    Standby(String),
    /// 503 — the store is sealed or unavailable. Nothing the caller does helps;
    /// somebody has to unseal it.
    Sealed(String),
    /// Anything else, including a body that did not parse.
    Unexpected { status: u16, message: String },
    /// The request never reached the server.
    Transport(String),
    /// The store answered successfully, with something this driver cannot read.
    ///
    /// Distinct from every variant above, which are the store saying no. This
    /// one is the store saying yes and handing over a shape we did not expect —
    /// a KV version 1 mount read as version 2, or credentials with no password
    /// in them. Retrying cannot help, and treating it as a plain failure would
    /// hide the one detail that explains it: which path, and what was wrong.
    Malformed { path: String, message: String },
}

impl VaultError {
    /// Build from a status code and whatever body came with it.
    pub fn from_response(status: u16, path: &str, body: &str) -> VaultError {
        let message = messages(body);

        match status {
            400 => VaultError::BadRequest(message),
            // 401 is not in Vault's vocabulary — it answers 403 for a missing
            // token as well as a forbidden one — but a proxy in front may
            // produce it, and treating it as anything else would be confusing.
            401 | 403 => VaultError::PermissionDenied(message),
            404 => VaultError::NotFound { path: path.to_string() },
            412 => VaultError::Stale(message),
            429 => VaultError::Standby(message),
            501 | 503 => VaultError::Sealed(message),
            other => VaultError::Unexpected { status: other, message },
        }
    }

    /// Whether trying again, unchanged, could plausibly work.
    ///
    /// A sealed store is deliberately *not* retryable: unsealing is a human
    /// action that takes minutes at best, and hammering the endpoint until it
    /// happens only buries the log line that would have explained the outage.
    pub fn is_retryable(&self) -> bool {
        matches!(self, VaultError::Stale(_) | VaultError::Standby(_) | VaultError::Transport(_))
    }

    /// Whether re-authenticating and trying again could work.
    pub fn needs_new_token(&self) -> bool {
        matches!(self, VaultError::PermissionDenied(_))
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, VaultError::NotFound { .. })
    }
}

/// Pull the messages out of `{"errors": ["...", "..."]}`.
///
/// Falls back to the raw body, trimmed: an HTML error page from a proxy in
/// front of Vault is not the shape we expect, and showing it is far more useful
/// than reporting "unknown error".
fn messages(body: &str) -> String {
    let parsed = Json::parse(body).ok();
    let errors = parsed
        .as_ref()
        .and_then(|json| json.get("errors"))
        .and_then(Json::as_array)
        .map(|items| {
            items.iter().filter_map(Json::as_str).collect::<Vec<_>>().join("; ")
        })
        .filter(|joined| !joined.is_empty());

    match errors {
        Some(errors) => errors,
        None => {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                "the server sent no explanation".to_string()
            } else if trimmed.len() > 300 {
                format!("{}…", &trimmed[..300])
            } else {
                trimmed.to_string()
            }
        }
    }
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::BadRequest(message) => write!(f, "the secret store rejected the request: {message}"),
            VaultError::PermissionDenied(message) => write!(
                f,
                "the secret store refused: {message}. The token is missing, expired, or has no \
                 policy granting this path — Vault answers the same way for all three."
            ),
            VaultError::NotFound { path } => {
                write!(f, "nothing is stored at `{path}`")
            }
            VaultError::Stale(message) => write!(
                f,
                "a standby node is behind and cannot serve this yet: {message}"
            ),
            VaultError::Standby(message) => {
                write!(f, "this node is a standby and will not serve the request: {message}")
            }
            VaultError::Sealed(message) => write!(
                f,
                "the secret store is sealed or unavailable: {message}. It has to be unsealed \
                 before anything here will work; retrying will not help."
            ),
            VaultError::Unexpected { status, message } => {
                write!(f, "the secret store answered {status}: {message}")
            }
            VaultError::Transport(message) => {
                write!(f, "could not reach the secret store: {message}")
            }
            VaultError::Malformed { path, message } => {
                write!(f, "the reply from `{path}` was not what this driver expected: {message}")
            }
        }
    }
}

impl std::error::Error for VaultError {}

impl From<VaultError> for Error {
    fn from(error: VaultError) -> Error {
        Error::msg(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_each_status_to_what_the_caller_should_do_about_it() {
        let body = r#"{"errors":["permission denied"]}"#;

        assert!(matches!(
            VaultError::from_response(403, "secret/data/x", body),
            VaultError::PermissionDenied(_)
        ));
        assert!(matches!(
            VaultError::from_response(404, "secret/data/x", "{}"),
            VaultError::NotFound { .. }
        ));
        assert!(matches!(
            VaultError::from_response(503, "sys/health", "{}"),
            VaultError::Sealed(_)
        ));
        assert!(matches!(
            VaultError::from_response(418, "secret/data/x", "{}"),
            VaultError::Unexpected { status: 418, .. }
        ));
    }

    #[test]
    fn a_missing_token_and_a_forbidden_one_look_the_same_because_they_are() {
        // Vault genuinely does not distinguish them. Inventing a distinction
        // here would mean lying about which one happened.
        assert_eq!(
            VaultError::from_response(401, "p", r#"{"errors":["x"]}"#),
            VaultError::from_response(403, "p", r#"{"errors":["x"]}"#),
        );
    }

    #[test]
    fn a_sealed_store_is_not_retried() {
        // Retrying cannot unseal it, and the retries would bury the one log
        // line that explains the outage.
        assert!(!VaultError::Sealed("sealed".into()).is_retryable());
        assert!(VaultError::Standby("standby".into()).is_retryable());
        assert!(VaultError::Stale("behind".into()).is_retryable());
        assert!(VaultError::Transport("refused".into()).is_retryable());
        assert!(!VaultError::BadRequest("bad".into()).is_retryable());
    }

    #[test]
    fn only_a_refusal_suggests_getting_a_new_token() {
        assert!(VaultError::PermissionDenied("x".into()).needs_new_token());
        assert!(!VaultError::NotFound { path: "p".into() }.needs_new_token());
    }

    #[test]
    fn reads_the_error_list_vault_actually_sends() {
        let error = VaultError::from_response(400, "p", r#"{"errors":["one","two"]}"#);
        assert!(error.to_string().contains("one; two"), "got {error}");
    }

    #[test]
    fn a_body_that_is_not_json_is_shown_rather_than_swallowed() {
        // A proxy in front of Vault answering with HTML is exactly when the
        // body matters most.
        let error = VaultError::from_response(502, "p", "<html>Bad Gateway</html>");
        assert!(error.to_string().contains("Bad Gateway"), "got {error}");
    }

    #[test]
    fn an_empty_body_says_so_instead_of_pretending() {
        let error = VaultError::from_response(500, "p", "");
        assert!(error.to_string().contains("no explanation"), "got {error}");
    }

    #[test]
    fn a_huge_body_is_truncated_so_it_cannot_flood_a_log() {
        let error = VaultError::from_response(500, "p", &"x".repeat(5000));
        assert!(error.to_string().len() < 500, "an error must not be a log flood");
    }

    #[test]
    fn a_not_found_names_the_path_because_that_is_the_useful_part() {
        let error = VaultError::from_response(404, "secret/data/missing", "{}");
        assert!(error.to_string().contains("secret/data/missing"));
    }

    #[test]
    fn an_errors_field_that_is_empty_falls_back_to_the_body() {
        // Vault sends `{"errors":[]}` for some 404s, which would otherwise
        // render as an error with no text at all.
        let error = VaultError::from_response(404, "p", r#"{"errors":[]}"#);
        assert!(!error.to_string().is_empty());
    }
}
