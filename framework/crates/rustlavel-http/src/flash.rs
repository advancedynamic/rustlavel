//! Leaving something for the next request from the same visitor.
//!
//! A form that fails validation has to answer twice over: tell the person what
//! is wrong, and give them back what they typed. Neither can travel in the
//! response, because the answer to a failed `POST` is a redirect — sending the
//! page directly would leave the browser on a URL that re-submits the form when
//! reloaded, which is the double-charge problem in miniature.
//!
//! So the errors and the old input are left where the *next* request will find
//! them, and the browser is sent back to the form.
//!
//! The trait is here rather than in `rustlavel-auth` on purpose. Validation
//! needs somewhere to leave errors and must not therefore depend on sessions;
//! the view layer needs to read them and must not depend on either. A session
//! registers itself as `Arc<dyn Flash>` on the request, and everything else
//! talks to that.

use crate::request::Request;
use rustlavel_core::Json;
use std::sync::Arc;

/// The key a failed validation leaves its messages under.
pub const ERRORS_KEY: &str = "_errors";

/// The key it leaves the submitted input under.
pub const OLD_INPUT_KEY: &str = "_old";

/// The key the session middleware records the last page under, so a failed
/// form knows where "back" is.
pub const PREVIOUS_URL_KEY: &str = "_previous";

/// Somewhere a value can be left for exactly one further request.
///
/// Implemented by the session. Anything reading through this trait works
/// whether or not sessions are enabled — with no session, `flash` is a no-op
/// and `take` finds nothing, which degrades a failed form to a plain `422`
/// rather than to a panic.
pub trait Flash: std::fmt::Debug + Send + Sync + 'static {
    /// Leave a value for the next request, and no longer.
    fn flash(&self, key: &str, value: Json);

    /// Read a value and consume it.
    fn take(&self, key: &str) -> Option<Json>;

    /// Read a value and leave it in place.
    ///
    /// A template renders the errors and the old input separately, and a page
    /// with two forms on it reads them more than once, so reading must not be
    /// what removes them. The flash lifetime does that at the end of the
    /// request instead.
    fn peek(&self, key: &str) -> Option<Json>;
}

impl Request {
    /// The flash store for this request, when something registered one.
    pub fn flash(&self) -> Option<&Arc<dyn Flash>> {
        self.extension::<Arc<dyn Flash>>()
    }

    /// The validation messages from the request that redirected here, as
    /// `{"email": ["…"]}` — empty when the last request did not fail.
    ///
    /// Hand it to a template and read one field with a dotted path:
    ///
    /// ```ignore
    /// req.view("posts/create", &ViewContext::new()
    ///     .with("errors", req.errors())
    ///     .with("old", req.old()))
    /// ```
    /// ```html
    /// @if(errors.title)<p class="error">{{ errors.title.0 }}</p>@endif
    /// <input name="title" value="{{ old.title }}">
    /// ```
    pub fn errors(&self) -> Json {
        self.flash()
            .and_then(|flash| flash.peek(ERRORS_KEY))
            .unwrap_or_else(|| Json::object([] as [(&str, Json); 0]))
    }

    /// The input the failed request submitted, so a form can refill itself.
    ///
    /// Never contains a password: [`old_input_of`] leaves those out, because
    /// re-filling a password field means putting the password back into HTML
    /// that ends up in caches, in history and in screenshots.
    pub fn old(&self) -> Json {
        self.flash()
            .and_then(|flash| flash.peek(OLD_INPUT_KEY))
            .unwrap_or_else(|| Json::object([] as [(&str, Json); 0]))
    }

    /// One field of the old input, as a string. Empty when there is none.
    pub fn old_field(&self, name: &str) -> String {
        self.old().get(name).and_then(Json::as_str).unwrap_or_default().to_string()
    }

    /// Whether the last request left validation messages behind.
    pub fn has_errors(&self) -> bool {
        self.errors().as_object().is_some_and(|fields| !fields.is_empty())
    }

    /// Where a failed form should send the browser back to.
    ///
    /// The page the session last recorded, then the `Referer`, then `/`. Both
    /// candidates are checked to be a path on this site: a full URL here would
    /// be an open redirect, which is how a phishing link borrows a real domain.
    pub fn previous_url(&self) -> String {
        let recorded = self
            .flash()
            .and_then(|flash| flash.peek(PREVIOUS_URL_KEY))
            .and_then(|value| value.as_str().map(str::to_string));

        recorded
            .or_else(|| self.header("referer").map(str::to_string))
            .filter(|target| is_local_path(target))
            .unwrap_or_else(|| "/".to_string())
    }
}

/// Whether a redirect target is a path on this site rather than another origin.
///
/// `//evil.example` is the one that catches people out: it has no scheme, looks
/// like a path, and a browser reads it as a protocol-relative URL to somebody
/// else's host.
pub fn is_local_path(target: &str) -> bool {
    target.starts_with('/') && !target.starts_with("//") && !target.contains('\\')
}

/// The submitted fields worth keeping, as a JSON object.
///
/// Anything whose name looks like a secret is dropped. The check is on the
/// name rather than the value because there is nothing about a password that
/// makes it recognisable — and the cost of guessing wrong in this direction is
/// only an empty field, while guessing wrong in the other puts a password in
/// the HTML.
pub fn old_input_of(request: &mut Request) -> Json {
    let sensitive = |name: &str| {
        let name = name.to_ascii_lowercase();
        ["password", "secret", "token", "_token", "otp", "code", "pin", "cvv", "card"]
            .iter()
            .any(|needle| name.contains(needle))
    };

    let pairs: Vec<(String, Json)> = request
        .form()
        .iter()
        .filter(|(name, _)| !sensitive(name))
        .map(|(name, value)| (name.clone(), Json::from(value.as_str())))
        .collect();

    Json::object(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::method::Method;
    use std::sync::Mutex;

    /// A flash store standing in for a session.
    #[derive(Debug, Default)]
    struct Notebook(Mutex<std::collections::BTreeMap<String, Json>>);

    impl Flash for Notebook {
        fn flash(&self, key: &str, value: Json) {
            self.0.lock().unwrap().insert(key.to_string(), value);
        }
        fn take(&self, key: &str) -> Option<Json> {
            self.0.lock().unwrap().remove(key)
        }
        fn peek(&self, key: &str) -> Option<Json> {
            self.0.lock().unwrap().get(key).cloned()
        }
    }

    fn with_flash(request: Request, notebook: Notebook) -> Request {
        let mut request = request;
        let store: Arc<dyn Flash> = Arc::new(notebook);
        request.extend(store);
        request
    }

    #[test]
    fn a_request_with_no_flash_reports_empty_rather_than_failing() {
        let request = Request::new(Method::Get, "/posts/create");
        assert!(!request.has_errors());
        assert_eq!(request.errors().as_object().map(|f| f.len()), Some(0));
        assert_eq!(request.old_field("title"), "");
        assert_eq!(request.previous_url(), "/");
    }

    #[test]
    fn errors_and_old_input_survive_to_the_next_request() {
        let notebook = Notebook::default();
        notebook.flash(
            ERRORS_KEY,
            Json::object([("title", Json::Array(vec![Json::from("The title field is required.")]))]),
        );
        notebook.flash(OLD_INPUT_KEY, Json::object([("body", Json::from("half a draft"))]));

        let request = with_flash(Request::new(Method::Get, "/posts/create"), notebook);

        assert!(request.has_errors());
        assert_eq!(
            request.errors().get("title.0").and_then(Json::as_str),
            Some("The title field is required.")
        );
        assert_eq!(request.old_field("body"), "half a draft");
        assert_eq!(request.old_field("title"), "", "a field with no old value is empty, not missing");
    }

    #[test]
    fn reading_does_not_consume_them() {
        // A page with two forms reads the bag more than once, and the second
        // read must find what the first did.
        let notebook = Notebook::default();
        notebook.flash(ERRORS_KEY, Json::object([("a", Json::Array(vec![Json::from("x")]))]));
        let request = with_flash(Request::new(Method::Get, "/"), notebook);

        assert!(request.has_errors());
        assert!(request.has_errors());
    }

    #[test]
    fn old_input_keeps_what_was_typed_and_drops_what_was_secret() {
        let mut request = Request::new(Method::Post, "/register")
            .with_header("content-type", "application/x-www-form-urlencoded")
            .with_body(
                b"name=Ada&email=ada%40example.com&password=hunter2&\
                  password_confirmation=hunter2&_token=abc&api_token=xyz&note=fine"
                    .to_vec(),
            );

        let old = old_input_of(&mut request);
        assert_eq!(old.get("name").and_then(Json::as_str), Some("Ada"));
        assert_eq!(old.get("email").and_then(Json::as_str), Some("ada@example.com"));
        assert_eq!(old.get("note").and_then(Json::as_str), Some("fine"));

        for secret in ["password", "password_confirmation", "_token", "api_token"] {
            assert!(old.get(secret).is_none(), "{secret} must not be kept");
        }
    }

    #[test]
    fn back_goes_to_the_recorded_page_then_the_referer_then_the_root() {
        let notebook = Notebook::default();
        notebook.flash(PREVIOUS_URL_KEY, Json::from("/posts/create"));
        let request = with_flash(
            Request::new(Method::Post, "/posts").with_header("referer", "/somewhere-else"),
            notebook,
        );
        assert_eq!(request.previous_url(), "/posts/create", "the recorded page wins");

        let no_record = Request::new(Method::Post, "/posts").with_header("referer", "/from-here");
        assert_eq!(no_record.previous_url(), "/from-here");

        let nothing = Request::new(Method::Post, "/posts");
        assert_eq!(nothing.previous_url(), "/");
    }

    #[test]
    fn a_referer_pointing_at_another_site_is_refused() {
        // Sending the browser wherever the Referer says is an open redirect,
        // and the header is written by whoever linked to the form.
        for hostile in [
            "https://evil.example/login",
            "//evil.example/login",
            "http://evil.example",
            "/\\evil.example",
        ] {
            let request = Request::new(Method::Post, "/posts").with_header("referer", hostile);
            assert_eq!(request.previous_url(), "/", "{hostile} should not be followed");
        }
    }

    #[test]
    fn a_local_path_is_recognised_and_a_foreign_one_is_not() {
        assert!(is_local_path("/posts/create"));
        assert!(is_local_path("/"));
        assert!(!is_local_path("//evil.example"));
        assert!(!is_local_path("https://evil.example"));
        assert!(!is_local_path("posts/create"));
        assert!(!is_local_path("/\\evil.example"));
    }
}
