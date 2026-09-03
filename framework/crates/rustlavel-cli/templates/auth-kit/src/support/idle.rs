//! Signing somebody out after a stretch of doing nothing.
//!
//! The session store already has a lifetime, but it is fixed when the store is
//! built and a session is only rewritten when something in it changed — so a
//! person reading page after page without touching anything can age out of a
//! store that was never meant to expire them, and an administrator moving
//! Settings → Security → *Session Timeout* would be changing a number that
//! nothing reads until the next restart. This middleware is the part that
//! reads it on every request.

use rustlavel::prelude::*;

use crate::support::settings::Settings;
use crate::support::{page, tokens};

/// The session key holding the last time this person did anything.
const LAST_SEEN: &str = "last_seen";

/// How stale `last_seen` is allowed to get before it is rewritten.
///
/// Writing it on every request would mean a store write on every request, for
/// a value only ever compared in minutes. A minute of slack costs nothing and
/// leaves an idle reader's session untouched.
const GRANULARITY: i64 = 60;

/// Ends a session that has been idle longer than `auth.session.timeout`.
///
/// Belongs after `Authenticate` in a group: it has nothing to say about a
/// request that was not signed in to begin with.
pub struct IdleTimeout;

impl Middleware for IdleTimeout {
    fn handle(&self, request: Request, next: Next) -> BoxFuture<Response> {
        Box::pin(async move {
            let minutes = timeout_minutes(&request).await;
            let now = tokens::unix_now();

            if let Some(session) = request.try_session() {
                let last_seen = session.get(LAST_SEEN).and_then(|v| v.as_f64()).unwrap_or(0.0) as i64;

                // Zero means never, which is what a site behind a VPN wants and
                // what the choices on the Security tab offer as "Never".
                if minutes > 0 && last_seen > 0 && now - last_seen > minutes * 60 {
                    Guard::new(session.clone()).logout();
                    page::flash(&request, "info", "You were signed out after a period of inactivity.");
                    return Response::see_other("/login");
                }

                if now - last_seen >= GRANULARITY {
                    session.put(LAST_SEEN, Json::from(now as f64));
                }
            }

            next.run(request).await
        })
    }
}

/// The timeout in minutes, as Settings → Security has it. Zero is never.
async fn timeout_minutes(req: &Request) -> i64 {
    let raw = match req.state::<Settings>() {
        Some(settings) => settings.get("auth.session.timeout").await,
        None => req.config().string("auth.session.timeout", "0"),
    };
    raw.parse::<i64>().unwrap_or(0).max(0)
}
