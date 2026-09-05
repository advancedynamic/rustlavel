//! The response headers a browser needs in order to enforce anything.
//!
//! **The policy this application was written against did not exist.** Five
//! places in the kit — the stylesheet, the appearance tab, the layout — explain
//! that they avoid an inline `<style>` or an `on click` handler *because* the
//! pages are served under a Content-Security-Policy with no `unsafe-inline`.
//! The discipline was real and held: no inline style, no inline script, no
//! event-handler attributes, no external origin. The header was never sent, so
//! none of it bought anything. A browser enforces a policy it is given.
//!
//! It is given one now, and the discipline is what makes it safe to turn on:
//! this is a policy the pages already satisfy, not a policy they will have to
//! be rewritten for.
//!
//! `default-src 'self'` and `object-src 'none'` are the two that matter for
//! script injection — together they mean a `<script>` an attacker gets into the
//! page does not run unless it came from this origin. `frame-ancestors 'none'`
//! is clickjacking; `form-action 'self'` stops a planted form posting somebody's
//! session elsewhere.

use rustlavel::prelude::*;

/// The policy, as one header value.
///
/// `data:` is allowed for images only, because a QR code and an uploaded logo
/// can arrive that way. It is not allowed for scripts, which is where `data:`
/// is dangerous.
pub const POLICY: &str = "default-src 'self'; \
                          img-src 'self' data:; \
                          object-src 'none'; \
                          base-uri 'self'; \
                          frame-ancestors 'none'; \
                          form-action 'self'";

/// Sets the security headers on every response that does not already carry them.
///
/// Does not overwrite: a route that sets a narrower policy of its own — the
/// logo endpoint does — has thought about its own case, and a blanket
/// middleware should not undo that.
pub struct SecurityHeaders;

impl Middleware for SecurityHeaders {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        Box::pin(async move {
            let mut response = next.run(request).await;

            for (name, value) in [
                ("content-security-policy", POLICY),
                // Stops a browser guessing that a text file is JavaScript,
                // which is how an upload becomes a script.
                ("x-content-type-options", "nosniff"),
                // The full URL of an admin page is not somebody else's
                // business; a path can carry an id.
                ("referrer-policy", "strict-origin-when-cross-origin"),
                // For browsers that do not implement `frame-ancestors`.
                ("x-frame-options", "DENY"),
            ] {
                if response.headers.get(name).is_none() {
                    response = response.with_header(name, value);
                }
            }
            response
        })
    }
}
