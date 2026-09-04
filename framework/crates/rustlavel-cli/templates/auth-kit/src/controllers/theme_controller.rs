//! The stylesheet the Appearance settings produce.
//!
//! The colours an administrator picks have to reach the page somehow, and the
//! obvious way — a `<style>` block in the layout — is exactly what this
//! application's Content-Security-Policy forbids. So they are served as a
//! stylesheet from this origin instead, which the policy allows and a browser
//! caches.
//!
//! It is a route rather than a file on disk because a file would have to be
//! rewritten on save, and then kept in step across however many processes are
//! serving the application. A route reads the settings, which are already
//! shared and already invalidated on write.

use rustlavel::prelude::*;

use crate::support::settings::Settings;

pub struct ThemeController;

impl ThemeController {
    /// `GET /css/theme.css`
    pub async fn stylesheet(req: Request) -> Result<Response> {
        let Some(settings) = req.state::<Settings>() else {
            return Ok(css_response(&req, String::new()));
        };

        let mut css = String::from(
            "/* Generated from Settings → Appearance. Edit it there, not here. */\n:root {\n",
        );

        // The brand ramp first, because it is the part that reaches every page
        // rather than one surface. Tailwind emits its utilities as
        // `var(--color-brand-600)`, so redefining the variables here is enough
        // to repaint every button, link, ring and badge in the application —
        // no rebuild, and nothing in the markup has to know.
        css.push_str(&crate::support::palette::brand_ramp(&settings.get("theme.brand").await));

        for (variable, key) in [
            ("--login-from", "theme.login.light.from"),
            ("--login-to", "theme.login.light.to"),
            ("--sidebar-bg", "theme.sidebar.light.bg"),
            ("--sidebar-text", "theme.sidebar.light.text"),
            ("--sidebar-active-bg", "theme.sidebar.light.active_bg"),
            ("--sidebar-active-text", "theme.sidebar.light.active_text"),
        ] {
            css.push_str(&format!("  {variable}: {};\n", colour(&settings.get(key).await)));
        }
        css.push_str("}\n\n");

        // The same three-state pattern the rest of the stylesheet uses: the
        // media query is guarded so an explicit light choice still beats a dark
        // operating system, and the attribute repeats it so the toggle wins in
        // the other direction.
        let dark = {
            let mut block = String::new();
            for (variable, key) in [
                ("--login-from", "theme.login.dark.from"),
                ("--login-to", "theme.login.dark.to"),
                ("--sidebar-bg", "theme.sidebar.dark.bg"),
                ("--sidebar-text", "theme.sidebar.dark.text"),
                ("--sidebar-active-bg", "theme.sidebar.dark.active_bg"),
                ("--sidebar-active-text", "theme.sidebar.dark.active_text"),
            ] {
                block.push_str(&format!("    {variable}: {};\n", colour(&settings.get(key).await)));
            }
            block
        };

        css.push_str(&format!(
            "@media (prefers-color-scheme: dark) {{\n  :root:not([data-theme=\"light\"]) {{\n{dark}  }}\n}}\n\n\
             :root.dark {{\n{dark}}}\n"
        ));

        Ok(css_response(&req, css))
    }
}

/// A stylesheet response, with a content type a browser will honour.
///
/// **The order matters and it is not obvious.** `Response::with_text` sets
/// `content-type: text/plain` itself, so putting it after `.with_header(...)`
/// silently replaces the type — and a browser in standards mode refuses to
/// apply a stylesheet served as plain text, with no error anywhere except a
/// console warning nobody was looking at. That is how every colour on the
/// Appearance tab came to do nothing at all.
fn css_response(req: &Request, css: String) -> Response {
    // Weak, because two runs that produce the same bytes are the same
    // stylesheet whatever else changed around them.
    let etag = format!("W/\"{:016x}\"", fingerprint(css.as_bytes()));

    // The browser asking "still this one?" gets an answer with no body. An
    // ETag nothing acts on is decoration, and this application registers no
    // ETag middleware to act on it — so the check belongs here, beside the
    // hash it compares.
    if req.header("if-none-match").is_some_and(|sent| sent.split(',').any(|one| one.trim() == etag)) {
        return Response::new(Status::NOT_MODIFIED)
            .with_header("cache-control", "no-cache")
            .with_header("etag", etag);
    }

    Response::ok()
        .with_body(css.into_bytes())
        .with_header("content-type", "text/css; charset=utf-8")
        // **Revalidate every time.** This file is generated from Settings →
        // Appearance, so it changes the moment somebody clicks Save — and it
        // used to say `max-age=60`, which meant the browser kept painting the
        // old colours from its own cache and the tab looked broken. A minute
        // is long enough for a person to decide the feature does not work, and
        // Chrome's memory cache can hold a parsed stylesheet longer than that
        // again. It is about a kilobyte; asking is cheap, and the ETag turns
        // the usual answer into a 304 with no body at all.
        .with_header("cache-control", "no-cache")
        .with_header("etag", etag)
}

/// A 64-bit FNV-1a of the generated stylesheet.
///
/// Not a cryptographic hash and not trying to be: this decides whether a
/// browser already has these bytes, and the cost of a collision is a stale
/// colour until the next save.
fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A stored colour, or a safe one.
///
/// Public because the settings form validates with it on the way in as well.
/// Two nets rather than one: this is the last thing between a stored value and
/// a stylesheet, and it must not be the only thing.
///
/// Everything here is written straight into a stylesheet, so a value that is
/// not a colour is a value that could close the declaration and open something
/// else. Six or three hex digits, and nothing else gets through.
pub fn colour(value: &str) -> String {
    let candidate = value.trim();
    let body = candidate.strip_prefix('#').unwrap_or("");
    let valid = matches!(body.len(), 3 | 6) && body.chars().all(|c| c.is_ascii_hexdigit());

    if valid { format!("#{body}") } else { "#000000".to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A browser refuses a stylesheet served as `text/plain` and says so only
    /// in a console warning. Nothing else in this application notices, which
    /// is why the type is asserted rather than assumed.
    #[test]
    fn the_stylesheet_is_served_as_css_not_as_plain_text() {
        let response = css_response(&Request::new(Method::Get, "/css/theme.css"), "body{}".to_string());

        assert_eq!(response.headers.get("content-type"), Some("text/css; charset=utf-8"));
        assert_eq!(response.body_string(), "body{}");
    }

    /// The colours change on Save, so the answer must not come from a cache
    /// this application cannot reach into. It used to say `max-age=60`, and a
    /// person who pressed Save saw the old colours and concluded the tab was
    /// broken.
    #[test]
    fn the_stylesheet_is_revalidated_rather_than_held() {
        let request = Request::new(Method::Get, "/css/theme.css");
        let response = css_response(&request, "body{}".to_string());

        assert_eq!(response.headers.get("cache-control"), Some("no-cache"));
        let etag = response.headers.get("etag").expect("no etag").to_string();

        // And the revalidation it invites is answered without a body.
        let conditional =
            Request::new(Method::Get, "/css/theme.css").with_header("if-none-match", etag.clone());
        let again = css_response(&conditional, "body{}".to_string());
        assert_eq!(again.status, Status::NOT_MODIFIED);
        assert!(again.body_string().is_empty(), "a 304 carries no body");

        // Different colours are a different stylesheet.
        let other = css_response(&conditional, "body{color:red}".to_string());
        assert_ne!(other.status, Status::NOT_MODIFIED, "changed CSS must not answer 304");
    }

    use super::colour;

    #[test]
    fn only_a_hex_colour_reaches_the_stylesheet() {
        assert_eq!(colour("#3b82f6"), "#3b82f6");
        assert_eq!(colour("  #FFF  "), "#FFF");

        // The reason this function exists: anything else would be written
        // verbatim into a stylesheet.
        for hostile in [
            "red; } body { display: none",
            "#fff; background: url(https://evil.example/x)",
            "url(javascript:alert(1))",
            "",
            "#12345",
            "#gggggg",
        ] {
            assert_eq!(colour(hostile), "#000000", "{hostile} should not have been accepted");
        }
    }
}
