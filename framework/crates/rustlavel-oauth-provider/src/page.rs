//! The two pages this server renders itself: the consent screen, and the
//! error page for a request that must not be redirected anywhere.
//!
//! The error page exists for one reason. When `redirect_uri` is missing,
//! malformed, or not one the client registered, there is nowhere safe to send
//! the user — the only URI available is the attacker's. Redirecting the error
//! *is* the open redirect. So those failures are rendered here, on our own
//! origin, where the worst outcome is a page nobody wanted to read.

use crate::client::Client;
use rustlavel_http::{Response, Status};
use rustlavel_oauth::{OAuthError, Scopes};

/// Escape text for HTML.
///
/// A client name and a scope both reach these pages from a registration this
/// server did not write, and a `<script>` in a client's name would otherwise
/// run on the origin that issues tokens.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

const STYLE: &str = "\
:root{color-scheme:light dark}\
body{margin:0;min-height:100vh;display:grid;place-items:center;\
font:15px/1.6 ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif;\
background:#f6f6f7;color:#18181b}\
main{max-width:34rem;width:calc(100% - 3rem);background:#fff;border:1px solid #e4e4e7;\
border-radius:12px;padding:2rem;box-shadow:0 1px 3px rgba(0,0,0,.06)}\
h1{margin:0 0 .5rem;font-size:1.25rem;letter-spacing:-.01em}\
p{margin:0 0 1rem;color:#52525b}\
code{font:13px ui-monospace,SFMono-Regular,Menlo,monospace;background:#f4f4f5;\
padding:.1rem .35rem;border-radius:4px}\
ul{margin:0 0 1.5rem;padding-left:1.1rem;color:#3f3f46}\
li{margin:.25rem 0}\
form{display:flex;gap:.75rem}\
button{flex:1;font:inherit;font-weight:500;padding:.6rem 1rem;border-radius:8px;cursor:pointer}\
.approve{background:#18181b;color:#fff;border:1px solid #18181b}\
.deny{background:#fff;color:#3f3f46;border:1px solid #d4d4d8}\
@media(prefers-color-scheme:dark){\
body{background:#09090b;color:#fafafa}\
main{background:#18181b;border-color:#27272a;box-shadow:none}\
p{color:#a1a1aa}ul{color:#d4d4d8}code{background:#27272a}\
.approve{background:#fafafa;color:#09090b;border-color:#fafafa}\
.deny{background:#18181b;color:#d4d4d8;border-color:#3f3f46}}";

fn document(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{STYLE}</style></head><body><main>{body}</main></body></html>",
        title = escape(title),
    )
}

/// The page for a request that cannot be redirected back to the client.
pub fn error(error: &OAuthError) -> Response {
    let description = error
        .description
        .clone()
        .unwrap_or_else(|| "the authorization request could not be processed".to_string());

    let body = format!(
        "<h1>This authorization request was refused</h1>\
         <p>{description}</p>\
         <p>Nothing was sent back to the application, because the request did not establish \
         where it would be safe to send it. If you are the developer of that application, the \
         error code is <code>{code}</code>.</p>",
        description = escape(&description),
        code = escape(error.code.as_str()),
    );

    Response::new(error.code.status())
        .with_html(document("Authorization refused", &body))
        // A refusal is about this request; caching one would answer for the next.
        .with_header("cache-control", "no-store")
}

/// The consent screen.
///
/// `action` is the path this posts back to, and the hidden fields carry the
/// request forward. They are *not* trusted on the way back: the POST handler
/// re-validates every one of them against the registered client, so a modified
/// hidden field buys nothing.
pub struct Consent<'a> {
    pub client: &'a Client,
    pub scopes: &'a Scopes,
    pub action: &'a str,
    pub redirect_uri: &'a str,
    pub state: Option<&'a str>,
    pub challenge: &'a str,
    pub challenge_method: &'a str,
    /// The application's own CSRF hidden input, if a session is in play.
    pub csrf_field: &'a str,
}

impl Consent<'_> {
    pub fn render(&self) -> Response {
        let scopes = if self.scopes.is_empty() {
            "<li>Nothing beyond confirming who you are</li>".to_string()
        } else {
            self.scopes
                .iter()
                .map(|scope| format!("<li><code>{}</code></li>", escape(scope)))
                .collect::<Vec<_>>()
                .join("")
        };

        let body = format!(
            "<h1>Authorize {name}?</h1>\
             <p><strong>{name}</strong> is asking for access to your account. It will be able to:</p>\
             <ul>{scopes}</ul>\
             <form method=\"post\" action=\"{action}\">\
             {csrf}\
             {hidden}\
             <button class=\"deny\" name=\"approve\" value=\"no\" type=\"submit\">Deny</button>\
             <button class=\"approve\" name=\"approve\" value=\"yes\" type=\"submit\">Authorize</button>\
             </form>",
            name = escape(&self.client.name),
            action = escape(self.action),
            csrf = self.csrf_field,
            hidden = self.hidden_fields(),
        );

        Response::html(document(&format!("Authorize {}", self.client.name), &body))
            // The screen carries the request's parameters; a cached copy shown
            // to the next visitor at this browser would carry them too.
            .with_header("cache-control", "no-store")
    }

    fn hidden_fields(&self) -> String {
        let mut fields = vec![
            ("response_type", "code"),
            ("client_id", self.client.id.as_str()),
            ("redirect_uri", self.redirect_uri),
            ("code_challenge", self.challenge),
            ("code_challenge_method", self.challenge_method),
        ];
        let scopes = self.scopes.to_string();
        fields.push(("scope", &scopes));
        if let Some(state) = self.state {
            fields.push(("state", state));
        }

        fields
            .iter()
            .map(|(name, value)| {
                format!(
                    "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                    escape(name),
                    escape(value)
                )
            })
            .collect()
    }
}

/// A redirect that carries an error back to a client, once — and only once —
/// the redirect URI has been checked against the registered set.
pub fn redirect_error(uri: &str, error: &OAuthError) -> Response {
    Response::new(Status::FOUND)
        .with_header("location", rustlavel_oauth::url::append_query(uri, &error.to_query()))
        .with_header("cache-control", "no-store")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_oauth::OAuthErrorCode;

    #[test]
    fn escaping_closes_every_way_out_of_an_attribute_or_a_tag() {
        assert_eq!(escape("<script>"), "&lt;script&gt;");
        assert_eq!(escape(r#"a"b'c&d"#), "a&quot;b&#39;c&amp;d");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn a_local_error_page_says_nothing_was_redirected() {
        let refusal =
            OAuthError::because(OAuthErrorCode::InvalidRequest, "unregistered redirect_uri");
        let body = super::error(&refusal).body_string();

        assert!(body.contains("Nothing was sent back"));
        assert!(body.contains("unregistered redirect_uri"));
        assert!(body.contains("invalid_request"));
        assert_eq!(super::error(&refusal).status, Status::BAD_REQUEST);
    }

    #[test]
    fn a_client_name_cannot_smuggle_markup_onto_the_consent_screen() {
        let client = Client::public("evil").named("<script>steal()</script>");
        let scopes = Scopes::of(["read"]);
        let consent = Consent {
            client: &client,
            scopes: &scopes,
            action: "/oauth/authorize",
            redirect_uri: "https://a.test/cb",
            state: Some(r#""><script>x</script>"#),
            challenge: "challenge",
            challenge_method: "S256",
            csrf_field: "",
        };

        let body = consent.render().body_string();
        assert!(!body.contains("<script>steal()"), "the name reached the page as markup");
        assert!(body.contains("&lt;script&gt;steal()"));
        assert!(!body.contains(r#""><script>x"#), "the state broke out of its attribute");
    }

    #[test]
    fn the_consent_form_carries_the_request_forward() {
        let client = Client::public("spa").named("Reader");
        let scopes = Scopes::of(["read", "write"]);
        let body = Consent {
            client: &client,
            scopes: &scopes,
            action: "/oauth/authorize",
            redirect_uri: "https://a.test/cb",
            state: None,
            challenge: "the-challenge",
            challenge_method: "S256",
            csrf_field: r#"<input type="hidden" name="_token" value="abc">"#,
        }
        .render()
        .body_string();

        assert!(body.contains(r#"name="client_id" value="spa""#));
        assert!(body.contains(r#"name="redirect_uri" value="https://a.test/cb""#));
        assert!(body.contains(r#"name="scope" value="read write""#));
        assert!(body.contains(r#"name="code_challenge" value="the-challenge""#));
        assert!(body.contains(r#"name="_token""#), "the application's CSRF field is rendered");
        assert!(body.contains(r#"value="yes""#) && body.contains(r#"value="no""#));
        assert!(!body.contains(r#"name="state""#), "no state, no field");
    }

    #[test]
    fn neither_page_may_be_cached() {
        let error = OAuthError::new(OAuthErrorCode::InvalidRequest);
        assert_eq!(super::error(&error).headers.get("cache-control"), Some("no-store"));

        let client = Client::public("spa");
        let scopes = Scopes::new();
        let response = Consent {
            client: &client,
            scopes: &scopes,
            action: "/oauth/authorize",
            redirect_uri: "https://a.test/cb",
            state: None,
            challenge: "c",
            challenge_method: "S256",
            csrf_field: "",
        }
        .render();
        assert_eq!(response.headers.get("cache-control"), Some("no-store"));
    }

    #[test]
    fn a_redirected_error_appends_to_a_uri_that_already_has_a_query() {
        let error = OAuthError::because(OAuthErrorCode::AccessDenied, "user said no")
            .with_state(Some("xyz".into()));

        let response = redirect_error("https://a.test/cb?ref=1", &error);
        let location = response.headers.get("location").expect("location");

        assert!(location.starts_with("https://a.test/cb?ref=1&error=access_denied"));
        assert!(location.contains("state=xyz"));
    }
}
