//! Acting as another user, and getting back.
//!
//! Support asks "what does this user see?" and the honest answer is usually
//! that nobody knows, because the bug depends on that user's data and that
//! user's permissions. Impersonation lets an administrator look — and the
//! whole design problem is making sure they can only look at what they are
//! allowed to, that they can always get back, and that the audit trail says
//! who was really at the keyboard.
//!
//! ```ignore
//! // Start, having already checked the administrator may do this:
//! Impersonation::start(session, "42")?;
//!
//! // Anywhere downstream:
//! if let Some(real) = req.impersonator() {
//!     // Draw the "you are viewing as …" banner. `req.identity()` is the
//!     // user being viewed; `real` is who is actually logged in.
//! }
//!
//! // Stop, which any impersonated session may do without a permission check:
//! Impersonation::stop(&session)?;
//! ```
//!
//! Three rules this enforces, each because the alternative is a real hole:
//!
//! - **The real identity is kept, never overwritten.** The session records who
//!   started the impersonation, so a log line can name them and so stopping is
//!   possible at all. An implementation that simply swapped the identifier
//!   would leave an administrator stuck as somebody else.
//! - **Impersonation does not nest.** Starting one while another is running is
//!   refused rather than stacked, because a stack is a way to lose track of who
//!   the real operator is — and the only thing that matters is the person at
//!   the keyboard, not the chain.
//! - **Stopping never needs a permission.** It restores the identity the
//!   session already holds. If it required a check, an administrator whose
//!   rights were revoked mid-session would be trapped as another user.
//!
//! **Who may impersonate is not decided here.** This module does not know what
//! a role is. The caller must check — with `rustlavel-rbac`, or by any other
//! rule — before calling [`Impersonation::start`], and must decide in
//! particular whether one administrator may impersonate another. Allowing that
//! turns any administrator account into every administrator account.

use crate::guard::Identity;
use crate::middleware::SessionHandle;
use rustlavel_core::{Error, Result};

/// The session key holding the real user's identifier while impersonating.
///
/// Deliberately not a secret and deliberately not signed: the session store
/// itself is the trust boundary, and a user who can write arbitrary session
/// data has already won.
const IMPERSONATOR_KEY: &str = "_impersonator";

/// Who is really at the keyboard, while somebody else is being viewed.
///
/// Attached to the request beside [`Identity`], which during impersonation
/// holds the user being viewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impersonator(String);

impl Impersonator {
    pub fn new(id: impl Into<String>) -> Self {
        Impersonator(id.into())
    }

    pub fn id(&self) -> &str {
        &self.0
    }

    pub fn id_as<T: std::str::FromStr>(&self) -> Option<T> {
        self.0.parse().ok()
    }
}

impl std::fmt::Display for Impersonator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct Impersonation;

impl Impersonation {
    /// Begin acting as `target`.
    ///
    /// The caller is responsible for having checked that the current user is
    /// allowed to. Fails when nobody is logged in, when impersonation is
    /// already running, or when the target is the current user — the last of
    /// those because it does nothing and leaves a session that looks
    /// impersonated but is not.
    pub fn start(session: &SessionHandle, target: impl Into<String>) -> Result<()> {
        let real = session
            .get_string(crate::guard::IDENTIFIER_KEY)
            .ok_or_else(|| Error::msg("nobody is logged in, so there is nobody to impersonate as"))?;

        if session.has(IMPERSONATOR_KEY) {
            return Err(Error::msg(
                "this session is already impersonating somebody; stop that first. \
                 Impersonation does not nest, so that the real operator is never in doubt.",
            ));
        }

        let target = target.into();
        if target == real {
            return Err(Error::msg("a user cannot impersonate themselves"));
        }

        // The session id is rotated, exactly as a login does. The identifier in
        // this session is about to change, and a session id that was valid
        // before the change must not still be valid after it.
        session.regenerate();
        session.put(IMPERSONATOR_KEY, real);
        session.put(crate::guard::IDENTIFIER_KEY, target);
        Ok(())
    }

    /// Stop, returning the identifier of the user who was being viewed.
    ///
    /// Never fails on a session that is not impersonating: this is the button
    /// a stuck operator presses, and it must not be able to refuse.
    pub fn stop(session: &SessionHandle) -> Option<String> {
        let real = session.get_string(IMPERSONATOR_KEY)?;
        let was = session.get_string(crate::guard::IDENTIFIER_KEY);

        session.regenerate();
        session.forget(IMPERSONATOR_KEY);
        session.put(crate::guard::IDENTIFIER_KEY, real);
        was
    }

    /// Whether this session is impersonating.
    pub fn is_impersonating(session: &SessionHandle) -> bool {
        session.has(IMPERSONATOR_KEY)
    }

    /// The real user's identifier, while impersonating.
    pub fn impersonator(session: &SessionHandle) -> Option<Impersonator> {
        session.get_string(IMPERSONATOR_KEY).map(Impersonator::new)
    }
}

/// Read the impersonator from a request, the way [`Identity`] is read.
pub trait ImpersonationExt {
    /// Who is really logged in, when the identity being used is somebody
    /// else's. `None` on an ordinary request.
    fn impersonator(&self) -> Option<&Impersonator>;

    /// The user actually accountable for this request: the impersonator when
    /// there is one, otherwise the logged-in user.
    ///
    /// **This is what an audit log should record.** Writing `Identity` alone
    /// means an administrator's actions are filed under the person they were
    /// viewing, which is precisely backwards.
    fn acting_user(&self) -> Option<String>;
}

impl ImpersonationExt for rustlavel_http::Request {
    fn impersonator(&self) -> Option<&Impersonator> {
        self.extension::<Impersonator>()
    }

    fn acting_user(&self) -> Option<String> {
        match self.extension::<Impersonator>() {
            Some(real) => Some(real.id().to_string()),
            None => self.extension::<Identity>().map(|id| id.id().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::AuthExt;
    use crate::key::AppKey;
    use crate::middleware::{SessionExt, SessionManager};
    use crate::store::MemoryStore;
    use rustlavel_http::{Method, Request, Response, Router, TestClient};

    /// A client whose one route reports who it thinks everybody is, and whose
    /// query string drives the impersonation.
    fn client() -> TestClient {
        let mut router = Router::new();
        router.middleware(SessionManager::new(&AppKey::from_bytes([5u8; 32]), MemoryStore::new()));
        router.get("/", |req: Request| async move {
            let session = req.session();
            match req.query("do") {
                Some("login") => {
                    crate::Guard::new(session.clone()).login_using_id(req.query("as").unwrap_or("1"));
                }
                Some("start") => {
                    if let Err(error) = Impersonation::start(session, req.query("as").unwrap_or("2")) {
                        return Response::text(format!("error: {error}"));
                    }
                }
                Some("stop") => {
                    Impersonation::stop(session);
                }
                _ => {}
            }
            let identity = session.get_string(crate::guard::IDENTIFIER_KEY).unwrap_or_else(|| "-".into());
            let real = Impersonation::impersonator(session).map(|i| i.id().to_string()).unwrap_or_else(|| "-".into());
            Response::text(format!("{identity}|{real}"))
        });
        TestClient::new(router)
    }

    #[tokio::test]
    async fn starting_swaps_the_identity_and_remembers_who_was_really_there() {
        let client = client();
        assert_eq!(client.get("/?do=login&as=7").await.body(), "7|-");
        assert_eq!(client.get("/?do=start&as=42").await.body(), "42|7", "viewing 42, really 7");
    }

    #[tokio::test]
    async fn stopping_puts_the_real_user_back() {
        let client = client();
        client.get("/?do=login&as=7").await;
        client.get("/?do=start&as=42").await;
        assert_eq!(client.get("/?do=stop").await.body(), "7|-");
        assert_eq!(client.get("/").await.body(), "7|-", "and it stays put");
    }

    #[tokio::test]
    async fn stopping_when_not_impersonating_does_nothing_and_does_not_fail() {
        // The stuck-operator button must never refuse.
        let client = client();
        client.get("/?do=login&as=7").await;
        assert_eq!(client.get("/?do=stop").await.body(), "7|-");
    }

    #[tokio::test]
    async fn it_does_not_nest() {
        let client = client();
        client.get("/?do=login&as=7").await;
        client.get("/?do=start&as=42").await;

        let response = client.get("/?do=start&as=99").await;
        assert!(response.body().contains("already impersonating"), "{}", response.body());
        assert_eq!(client.get("/").await.body(), "42|7", "and the first one is untouched");
    }

    #[tokio::test]
    async fn a_guest_cannot_impersonate() {
        let response = client().get("/?do=start&as=42").await;
        assert!(response.body().contains("nobody is logged in"), "{}", response.body());
    }

    #[tokio::test]
    async fn a_user_cannot_impersonate_themselves() {
        let client = client();
        client.get("/?do=login&as=7").await;
        let response = client.get("/?do=start&as=7").await;
        assert!(response.body().contains("cannot impersonate themselves"), "{}", response.body());
    }

    #[tokio::test]
    async fn the_session_id_changes_on_the_way_in_and_out() {
        // Same reason a login rotates it: an id that was valid while the
        // session meant one user must not still be valid once it means another.
        let client = client();
        client.get("/?do=login&as=7").await;
        let before = client.cookies().get("rustlavel_session").cloned();

        client.get("/?do=start&as=42").await;
        let during = client.cookies().get("rustlavel_session").cloned();
        assert_ne!(before, during, "the id must change when the identity does");

        client.get("/?do=stop").await;
        let after = client.cookies().get("rustlavel_session").cloned();
        assert_ne!(during, after, "and again on the way back");
    }

    #[test]
    fn an_audit_log_records_the_operator_not_the_person_being_viewed() {
        let plain = Request::new(Method::Get, "/");
        assert_eq!(plain.acting_user(), None);

        let mut logged_in = Request::new(Method::Get, "/");
        logged_in.extend(Identity::new("7"));
        assert_eq!(logged_in.acting_user().as_deref(), Some("7"));

        let mut viewing = Request::new(Method::Get, "/");
        viewing.extend(Identity::new("42"));
        viewing.extend(Impersonator::new("7"));
        assert_eq!(viewing.identity().map(|i| i.id().to_string()).as_deref(), Some("42"));
        assert_eq!(
            viewing.acting_user().as_deref(),
            Some("7"),
            "the administrator is accountable, not the user they were looking at"
        );
    }
}
