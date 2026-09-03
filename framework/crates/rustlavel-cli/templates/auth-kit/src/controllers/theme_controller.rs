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
            return Ok(Response::ok().with_header("content-type", "text/css").with_text(""));
        };

        let mut css = String::from(
            "/* Generated from Settings → Appearance. Edit it there, not here. */\n:root {\n",
        );

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

        Ok(Response::ok()
            .with_header("content-type", "text/css; charset=utf-8")
            // Short, because it changes when somebody clicks Save and they will
            // reload immediately to look at it.
            .with_header("cache-control", "public, max-age=60")
            .with_text(css))
    }
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
