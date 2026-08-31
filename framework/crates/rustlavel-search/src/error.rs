//! What a cluster says when it says no.
//!
//! Elasticsearch answers almost every failure with the same envelope —
//! `{"error": {"type": ..., "reason": ...}, "status": ...}` — and the status
//! code alone is not enough to act on. A 400 is both "your query has a syntax
//! error" and "this document does not fit the mapping"; a 404 is both "no such
//! index" and "no such document". The two halves of each pair call for
//! completely different responses, so the type is read as well as the code.
//!
//! The one distinction that costs money if it is lost: a 429 is the cluster
//! protecting itself and is worth retrying after a pause, while a mapping
//! error will fail identically forever. [`SearchError::is_retryable`] is the
//! only place that judgement is made.

use rustlavel_core::{Error, Json};

/// The crate's result type.
///
/// Deliberately not `rustlavel_core::Result`: telling a version conflict from
/// a mapping error is the entire point of this module, and flattening
/// everything into one message string would take that away from callers.
/// [`SearchError`] converts into the framework error with `?` when it reaches
/// a handler.
pub type Result<T> = std::result::Result<T, SearchError>;

/// A failure from Elasticsearch or OpenSearch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// The index does not exist.
    ///
    /// Usually a typo or a missing bootstrap step rather than an outage, and
    /// worth telling apart from an empty result: searching a misspelled index
    /// and searching an index with nothing in it look identical from the
    /// caller's side otherwise.
    IndexNotFound { index: String },
    /// The index already exists, from a create that assumed it did not.
    ///
    /// Ordinary in a startup path that creates its indices every boot, which
    /// is why [`crate::SearchClient::create_index_if_missing`] exists.
    IndexAlreadyExists { index: String },
    /// The document does not fit the mapping.
    ///
    /// A string where the mapping says `long`, a field the index refuses under
    /// `dynamic: strict`, an unparseable date. Never retryable — the same
    /// bytes will be rejected the same way for as long as the mapping stands.
    /// The reason carries the underlying cause where the cluster gives one,
    /// because `failed to parse field [price]` without it names the field but
    /// not the problem.
    MapperParsing { kind: String, reason: String },
    /// The document changed since it was read.
    ///
    /// Optimistic concurrency working as intended: something else wrote this
    /// document first. Retrying the identical request cannot help, but reading
    /// the document again and reapplying the change usually can — which is why
    /// this is not folded into [`SearchError::is_retryable`].
    VersionConflict { index: String, id: String, reason: String },
    /// The cluster refused the work to stay alive.
    ///
    /// A circuit breaker tripping, a full search or write queue, an index
    /// blocked by a disk watermark. The request was not performed, so backing
    /// off and repeating it is safe and is usually the correct response.
    Overloaded { kind: String, reason: String },
    /// 401 — no credentials, or credentials the cluster does not accept.
    Unauthorized { reason: String },
    /// 403 — authenticated, but not allowed to do this.
    ///
    /// Separate from [`SearchError::Unauthorized`] because the fixes have
    /// nothing in common: one is a wrong password, the other a missing role.
    Forbidden { reason: String },
    /// A request the cluster could parse but not accept — a malformed query,
    /// an illegal argument, an unknown parameter.
    BadRequest { kind: String, reason: String },
    /// The cluster is not answering: a gateway in front of it, a node
    /// restarting, a shard with no copy available.
    Unavailable { status: u16, reason: String },
    /// Some documents in a `_bulk` request were not written.
    ///
    /// The one failure the HTTP status will never tell you about: `_bulk`
    /// answers 200 whether every item succeeded or none did, and reports the
    /// per-item outcome in the body. This exists so that
    /// [`crate::BulkReport::ok_or_error`] can turn that into an error a `?`
    /// notices, for the callers that would rather not inspect every item.
    /// Which items, and why, is in the report — the reasons differ per item,
    /// so retryability cannot be decided for the request as a whole.
    BulkPartialFailure { failed: usize, total: usize, first: String },
    /// Anything else the cluster answered.
    Unexpected { status: u16, kind: String, reason: String },
    /// The request never reached the cluster.
    Transport(String),
    /// The cluster answered successfully with a shape this crate cannot read.
    ///
    /// Distinct from every variant above, which are the cluster saying no.
    /// This one is a yes in a form we did not expect — a proxy rewriting the
    /// body, or a version whose response layout moved — and naming what was
    /// being parsed is the only thing that makes it diagnosable.
    Malformed { context: String, message: String },
}

impl SearchError {
    /// Build from a status code and whatever body came with it.
    ///
    /// `context` names what was being attempted; it is used only where the
    /// body does not identify the resource itself, which is common for a
    /// document-level failure.
    pub fn from_response(status: u16, context: &str, body: &str) -> SearchError {
        let parsed = Json::parse(body).ok();
        let error = parsed.as_ref().and_then(|json| json.get("error"));

        let kind = error.and_then(|e| e.get("type")).and_then(Json::as_str).unwrap_or("").to_string();
        let reason = reason_of(error, body);
        let index = error
            .and_then(|e| e.get("index"))
            .and_then(Json::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| context.to_string());

        // The type is consulted before the status, because the interesting
        // pairs share a code: `mapper_parsing_exception` and a syntactically
        // broken query are both 400, and `index_not_found_exception` and a
        // missing document are both 404.
        match kind.as_str() {
            "index_not_found_exception" => SearchError::IndexNotFound { index },
            "resource_already_exists_exception" => SearchError::IndexAlreadyExists { index },
            "mapper_parsing_exception"
            | "document_parsing_exception"
            | "strict_dynamic_mapping_exception"
            | "mapper_exception" => SearchError::MapperParsing { kind, reason },
            "version_conflict_engine_exception" => SearchError::VersionConflict {
                index,
                id: error
                    .and_then(|e| e.get("id"))
                    .and_then(Json::as_str)
                    .map(str::to_string)
                    .unwrap_or_default(),
                reason,
            },
            // A circuit breaker can arrive as a 503 during a node restart as
            // well as the usual 429, so the type decides rather than the code.
            "circuit_breaking_exception"
            | "es_rejected_execution_exception"
            | "cluster_block_exception" => SearchError::Overloaded { kind, reason },
            "security_exception" if status == 403 => SearchError::Forbidden { reason },
            "security_exception" => SearchError::Unauthorized { reason },
            _ => match status {
                400 => SearchError::BadRequest { kind, reason },
                401 => SearchError::Unauthorized { reason },
                403 => SearchError::Forbidden { reason },
                404 => SearchError::IndexNotFound { index },
                409 => SearchError::VersionConflict { index, id: String::new(), reason },
                429 => SearchError::Overloaded { kind, reason },
                502..=504 => SearchError::Unavailable { status, reason },
                other => SearchError::Unexpected { status: other, kind, reason },
            },
        }
    }

    /// Whether repeating the identical request could plausibly succeed.
    ///
    /// The judgement callers most need and most often get wrong. A rejected
    /// write never happened, so repeating it after a pause is both safe and
    /// likely to work. A mapping error, a bad query and a permission failure
    /// will fail the same way for as long as nothing else changes, and
    /// retrying them only turns one clear log line into a hundred.
    ///
    /// A version conflict is deliberately *not* retryable even though a later
    /// attempt may well succeed: the request that lost carries a stale
    /// document, and sending it again would overwrite the winner.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            SearchError::Overloaded { .. }
                | SearchError::Unavailable { .. }
                | SearchError::Transport(_)
        )
    }

    /// Whether re-reading the document and reapplying the change could work.
    pub fn is_conflict(&self) -> bool {
        matches!(self, SearchError::VersionConflict { .. })
    }

    pub fn is_index_not_found(&self) -> bool {
        matches!(self, SearchError::IndexNotFound { .. })
    }

    /// Whether the failure is about credentials rather than the request.
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, SearchError::Unauthorized { .. } | SearchError::Forbidden { .. })
    }

    /// The cluster's own `error.type`, where it gave one.
    ///
    /// Worth surfacing: Elasticsearch has hundreds of exception types and this
    /// crate names only the handful worth branching on, so a caller chasing an
    /// unusual failure still has the string the documentation indexes by.
    pub fn kind(&self) -> &str {
        match self {
            SearchError::IndexNotFound { .. } => "index_not_found_exception",
            SearchError::IndexAlreadyExists { .. } => "resource_already_exists_exception",
            SearchError::VersionConflict { .. } => "version_conflict_engine_exception",
            SearchError::MapperParsing { kind, .. }
            | SearchError::Overloaded { kind, .. }
            | SearchError::BadRequest { kind, .. }
            | SearchError::Unexpected { kind, .. } => kind,
            _ => "",
        }
    }
}

/// Pull the human explanation out of the envelope.
///
/// `caused_by.reason` is appended where it says something new, because it is
/// routinely the only useful half: "failed to parse field [price] of type
/// [long]" names the field, and the cause names what was actually in it.
fn reason_of(error: Option<&Json>, body: &str) -> String {
    let Some(error) = error else {
        return fallback(body);
    };

    // Some failures — a wrong HTTP method, a few security responses — put a
    // bare string where the object normally goes.
    if let Some(text) = error.as_str() {
        return truncate(text);
    }

    let reason = error.get("reason").and_then(Json::as_str).unwrap_or("");
    let cause = error.get("caused_by.reason").and_then(Json::as_str).unwrap_or("");

    let joined = if cause.is_empty() || reason.contains(cause) {
        reason.to_string()
    } else {
        format!("{reason}: {cause}")
    };

    if joined.is_empty() { fallback(body) } else { truncate(&joined) }
}

/// Show the body when it is not the shape we expect.
///
/// A proxy answering with HTML in front of the cluster is exactly the case
/// where the raw body explains everything and "unknown error" explains nothing.
fn fallback(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "the cluster sent no explanation".to_string()
    } else {
        truncate(trimmed)
    }
}

/// Cap a reason so one error cannot flood a log.
///
/// Elasticsearch stack traces reach kilobytes, and `error_trace=true` makes
/// them longer still.
fn truncate(text: &str) -> String {
    if text.chars().count() <= 300 {
        return text.to_string();
    }
    let cut: String = text.chars().take(300).collect();
    format!("{cut}…")
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchError::IndexNotFound { index } => write!(
                f,
                "no index called `{index}`. Check the spelling, or create it before searching it — \
                 a missing index is not an empty one."
            ),
            SearchError::IndexAlreadyExists { index } => {
                write!(f, "the index `{index}` already exists")
            }
            SearchError::MapperParsing { kind, reason } => write!(
                f,
                "the document does not fit the mapping ({kind}): {reason}. Retrying will not \
                 help; either the document or the mapping has to change."
            ),
            SearchError::VersionConflict { index, id, reason } => write!(
                f,
                "`{id}` in `{index}` changed since it was read: {reason}. Read it again and \
                 reapply the change — sending this version again would overwrite the newer one."
            ),
            SearchError::Overloaded { kind, reason } => write!(
                f,
                "the cluster refused the request to protect itself ({kind}): {reason}. \
                 It was not performed; back off and try again."
            ),
            SearchError::Unauthorized { reason } => write!(
                f,
                "the cluster rejected the credentials: {reason}. Set a user and password, or an \
                 API key, on the client."
            ),
            SearchError::Forbidden { reason } => write!(
                f,
                "these credentials are not allowed to do that: {reason}. The password is \
                 accepted; the role is missing a privilege."
            ),
            SearchError::BadRequest { kind, reason } => {
                write!(f, "the cluster rejected the request ({kind}): {reason}")
            }
            SearchError::Unavailable { status, reason } => {
                write!(f, "the cluster is not answering ({status}): {reason}")
            }
            SearchError::BulkPartialFailure { failed, total, first } => write!(
                f,
                "{failed} of {total} bulk items were not written, starting with: {first}. \
                 The request itself answered 200 — inspect the report for the rest."
            ),
            SearchError::Unexpected { status, kind, reason } => {
                write!(f, "the cluster answered {status} ({kind}): {reason}")
            }
            SearchError::Transport(message) => {
                write!(f, "could not reach the cluster: {message}")
            }
            SearchError::Malformed { context, message } => write!(
                f,
                "the reply to `{context}` was not what this client expected: {message}"
            ),
        }
    }
}

impl std::error::Error for SearchError {}

impl From<SearchError> for Error {
    fn from(error: SearchError) -> Error {
        Error::msg(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bodies in these tests are copied from a running Elasticsearch 8.15,
    /// not written from memory.
    #[test]
    fn a_missing_index_is_told_apart_from_a_missing_document() {
        let body = r#"{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [nope]","resource.type":"index_or_alias","resource.id":"nope","index_uuid":"_na_","index":"nope"}],"type":"index_not_found_exception","reason":"no such index [nope]","resource.type":"index_or_alias","resource.id":"nope","index_uuid":"_na_","index":"nope"},"status":404}"#;

        assert_eq!(
            SearchError::from_response(404, "nope", body),
            SearchError::IndexNotFound { index: "nope".into() }
        );
    }

    #[test]
    fn a_mapping_failure_keeps_the_cause_because_the_reason_alone_names_no_problem() {
        let body = r#"{"error":{"root_cause":[{"type":"document_parsing_exception","reason":"[1:14] failed to parse field [price] of type [long] in document with id 'x'. Preview of field's value: 'cheap'"}],"type":"document_parsing_exception","reason":"[1:14] failed to parse field [price] of type [long] in document with id 'x'. Preview of field's value: 'cheap'","caused_by":{"type":"illegal_argument_exception","reason":"For input string: \"cheap\""}},"status":400}"#;

        let error = SearchError::from_response(400, "products/x", body);

        assert!(matches!(error, SearchError::MapperParsing { .. }));
        let text = error.to_string();
        assert!(text.contains("field [price]"), "got {text}");
        assert!(text.contains("For input string"), "the cause is the useful half: {text}");
    }

    #[test]
    fn a_mapping_error_is_never_retried_and_a_rejected_write_always_is() {
        // The distinction the whole module exists for: one of these will fail
        // identically forever, the other was never performed at all.
        let mapping = SearchError::MapperParsing { kind: "x".into(), reason: "y".into() };
        let rejected = SearchError::Overloaded { kind: "x".into(), reason: "y".into() };

        assert!(!mapping.is_retryable());
        assert!(rejected.is_retryable());
    }

    #[test]
    fn a_version_conflict_is_not_retryable_even_though_a_later_attempt_might_work() {
        // Retrying means resending the stale document, which would overwrite
        // whoever won the race. The caller has to re-read first.
        let conflict = SearchError::VersionConflict {
            index: "posts".into(),
            id: "1".into(),
            reason: "version conflict".into(),
        };

        assert!(!conflict.is_retryable());
        assert!(conflict.is_conflict());
    }

    #[test]
    fn a_version_conflict_names_the_document() {
        let body = r#"{"error":{"root_cause":[{"type":"version_conflict_engine_exception","reason":"[1]: version conflict, document already exists (current version [1])","index_uuid":"3zLDcCFFTASdSaKQTa93Xg","shard":"0","index":"posts"}],"type":"version_conflict_engine_exception","reason":"[1]: version conflict, document already exists (current version [1])","index_uuid":"3zLDcCFFTASdSaKQTa93Xg","shard":"0","index":"posts"},"status":409}"#;

        let error = SearchError::from_response(409, "posts", body);
        match &error {
            SearchError::VersionConflict { index, .. } => assert_eq!(index, "posts"),
            other => panic!("got {other:?}"),
        }
        assert!(error.to_string().contains("Read it again"), "{error}");
    }

    #[test]
    fn a_circuit_breaker_is_recognised_by_its_type_not_only_by_a_429() {
        let body = r#"{"error":{"root_cause":[{"type":"circuit_breaking_exception","reason":"[parent] Data too large, data for [<http_request>] would be [1.1gb], which is larger than the limit of [1gb]","bytes_wanted":1181116006,"bytes_limit":1073741824,"durability":"TRANSIENT"}],"type":"circuit_breaking_exception","reason":"[parent] Data too large","bytes_wanted":1181116006,"bytes_limit":1073741824,"durability":"TRANSIENT"},"status":429}"#;

        // The same breaker arrives as a 503 while a node is restarting.
        for status in [429, 503] {
            let error = SearchError::from_response(status, "posts", body);
            assert!(matches!(error, SearchError::Overloaded { .. }), "at {status}: {error:?}");
            assert!(error.is_retryable());
        }
    }

    #[test]
    fn bad_credentials_and_a_missing_privilege_are_different_problems() {
        let missing = r#"{"error":{"root_cause":[{"type":"security_exception","reason":"missing authentication credentials for REST request [/posts/_search]","header":{"WWW-Authenticate":["Basic realm=\"security\" charset=\"UTF-8\""]}}],"type":"security_exception","reason":"missing authentication credentials for REST request [/posts/_search]"},"status":401}"#;
        let denied = r#"{"error":{"root_cause":[{"type":"security_exception","reason":"action [indices:data/read/search] is unauthorized for user [reader] on indices [secret]"}],"type":"security_exception","reason":"action [indices:data/read/search] is unauthorized for user [reader] on indices [secret]"},"status":403}"#;

        assert!(matches!(
            SearchError::from_response(401, "posts", missing),
            SearchError::Unauthorized { .. }
        ));
        assert!(matches!(
            SearchError::from_response(403, "secret", denied),
            SearchError::Forbidden { .. }
        ));
        assert!(SearchError::from_response(403, "secret", denied).is_auth_failure());
    }

    #[test]
    fn a_creating_an_index_that_exists_is_its_own_case() {
        let body = r#"{"error":{"root_cause":[{"type":"resource_already_exists_exception","reason":"index [posts/9jNXbCyMS4WTRDMCsQiSuQ] already exists","index_uuid":"9jNXbCyMS4WTRDMCsQiSuQ","index":"posts"}],"type":"resource_already_exists_exception","reason":"index [posts/9jNXbCyMS4WTRDMCsQiSuQ] already exists","index_uuid":"9jNXbCyMS4WTRDMCsQiSuQ","index":"posts"},"status":400}"#;

        assert_eq!(
            SearchError::from_response(400, "posts", body),
            SearchError::IndexAlreadyExists { index: "posts".into() }
        );
    }

    #[test]
    fn a_broken_query_is_a_bad_request_and_not_a_mapping_error() {
        // Both are 400. Confusing them would tell somebody to change their
        // mapping when the query is what is wrong.
        let body = r#"{"error":{"root_cause":[{"type":"parsing_exception","reason":"unknown query [matchh]","line":1,"col":30}],"type":"parsing_exception","reason":"unknown query [matchh]","line":1,"col":30},"status":400}"#;

        let error = SearchError::from_response(400, "posts", body);
        assert!(matches!(error, SearchError::BadRequest { .. }), "got {error:?}");
        assert_eq!(error.kind(), "parsing_exception");
    }

    #[test]
    fn a_body_that_is_not_the_expected_envelope_is_shown_rather_than_swallowed() {
        let error = SearchError::from_response(502, "posts", "<html>502 Bad Gateway</html>");
        assert!(error.to_string().contains("Bad Gateway"), "got {error}");
    }

    #[test]
    fn a_bare_string_error_is_read_as_the_reason() {
        // Some responses put a string where the object normally goes.
        let error = SearchError::from_response(
            405,
            "posts",
            r#"{"error":"Incorrect HTTP method for uri [/posts] and method [PUT]","status":405}"#,
        );
        assert!(error.to_string().contains("Incorrect HTTP method"), "got {error}");
    }

    #[test]
    fn an_enormous_stack_trace_cannot_flood_a_log() {
        let body = format!(r#"{{"error":{{"type":"x","reason":"{}"}},"status":500}}"#, "y".repeat(9000));
        let error = SearchError::from_response(500, "posts", &body);
        assert!(error.to_string().len() < 500, "an error must not be a log flood");
    }

    #[test]
    fn an_empty_body_says_so_instead_of_pretending() {
        let error = SearchError::from_response(500, "posts", "");
        assert!(error.to_string().contains("no explanation"), "got {error}");
    }
}
