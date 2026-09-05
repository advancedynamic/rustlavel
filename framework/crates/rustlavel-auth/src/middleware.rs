//! The `session` middleware: cookie in, session out, and back again.
//!
//! ```ignore
//! let sessions = SessionManager::from_config(&config, FileStore::new("storage/sessions"))?;
//! router.middleware(sessions);
//! ```
//!
//! After that a handler reaches the session through the request:
//!
//! ```ignore
//! async fn show(req: Request) -> String {
//!     req.session().put("last_seen", "/dashboard");
//!     format!("visits: {:?}", req.session().get("visits"))
//! }
//! ```

use crate::key::AppKey;
use crate::session::{Session, is_valid_id};
use crate::store::{DEFAULT_LIFETIME, SessionStore, SharedStore};
use crate::{base64, constant_time_eq};
use hmac::{Hmac, Mac};
use rustlavel_core::{Config, Json, Result};
use rustlavel_http::handler::BoxFuture;
use rustlavel_http::middleware::{Middleware, Next};
use rustlavel_http::{Cookie, Method, Request, Response, SameSite};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

/// The cookie name used when nothing is configured.
pub const DEFAULT_COOKIE: &str = "rustlavel_session";

/// The request's session, shared between the middleware and the handler.
///
/// `Request::extension` hands out a shared reference, but the middleware has to
/// read the session back *after* the handler has changed it — so the session
/// lives behind a mutex and both sides hold the same handle. Cloning a handle
/// is cloning an `Arc`; every clone is the same session.
#[derive(Clone)]
pub struct SessionHandle(Arc<Mutex<Session>>);

impl SessionHandle {
    pub fn new(session: Session) -> Self {
        SessionHandle(Arc::new(Mutex::new(session)))
    }

    /// Borrow the session directly.
    ///
    /// The guard is not `Send`, so holding it across an `.await` will not
    /// compile — which is the behaviour we want: a session held open across an
    /// await is a session held open across another request.
    pub fn lock(&self) -> MutexGuard<'_, Session> {
        self.0.lock().expect("session lock poisoned")
    }

    /// Run a closure against the session, releasing the lock immediately.
    pub fn with<R>(&self, action: impl FnOnce(&mut Session) -> R) -> R {
        action(&mut self.lock())
    }

    pub fn id(&self) -> String {
        self.lock().id().to_string()
    }

    pub fn get(&self, key: &str) -> Option<Json> {
        self.lock().get(key).cloned()
    }

    pub fn get_string(&self, key: &str) -> Option<String> {
        self.lock().get_string(key)
    }

    pub fn has(&self, key: &str) -> bool {
        self.lock().has(key)
    }

    pub fn put(&self, key: impl Into<String>, value: impl Into<Json>) {
        self.lock().put(key, value);
    }

    pub fn forget(&self, key: &str) -> Option<Json> {
        self.lock().forget(key)
    }

    pub fn flush(&self) {
        self.lock().flush();
    }

    pub fn all(&self) -> BTreeMap<String, Json> {
        self.lock().all().clone()
    }

    pub fn flash(&self, key: impl Into<String>, value: impl Into<Json>) {
        self.lock().flash(key, value);
    }

    pub fn keep(&self, key: &str) {
        self.lock().keep(key);
    }

    pub fn reflash(&self) {
        self.lock().reflash();
    }

    /// Rotate the id, keeping the contents. See [`Session::regenerate`].
    pub fn regenerate(&self) -> String {
        self.lock().regenerate().to_string()
    }

    /// Empty the session and rotate the id.
    pub fn invalidate(&self) -> String {
        self.lock().invalidate().to_string()
    }

    /// The CSRF token, generated on first use.
    pub fn token(&self) -> String {
        self.lock().token()
    }
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle").field("id", &self.id()).finish()
    }
}

/// `req.session()` — reaching the session from a handler.
pub trait SessionExt {
    /// The session, or `None` when the `session` middleware is not installed.
    fn try_session(&self) -> Option<&SessionHandle>;

    /// The session, panicking with an actionable message if it is absent.
    fn session(&self) -> &SessionHandle;
}

impl SessionExt for Request {
    fn try_session(&self) -> Option<&SessionHandle> {
        self.extension::<SessionHandle>()
    }

    fn session(&self) -> &SessionHandle {
        self.try_session().expect(
            "no session on this request. Add the `session` middleware to the router \
             (`router.middleware(SessionManager::from_config(&config, store)?)`) before any route \
             that uses `req.session()`.",
        )
    }
}

/// Loads and stores the session around every request.
///
/// Cheap to clone; the clone shares one store.
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

struct Inner {
    store: SharedStore,
    cookie_key: [u8; 32],
    cookie: String,
    lifetime: Duration,
    path: String,
    secure: bool,
    same_site: SameSite,
}

/// The session is where a flashed value lives.
///
/// `flash` writes with the session's own one-request lifetime, so a value left
/// here is gone after the next request whether or not anybody read it — which
/// is what stops an old error message reappearing on a page three clicks later.
impl rustlavel_http::Flash for SessionHandle {
    fn flash(&self, key: &str, value: Json) {
        self.lock().flash(key, value);
    }

    fn take(&self, key: &str) -> Option<Json> {
        self.lock().forget(key)
    }

    fn peek(&self, key: &str) -> Option<Json> {
        self.lock().get(key).cloned()
    }
}

impl SessionManager {
    /// Build with an explicit key and store.
    pub fn new(key: &AppKey, store: impl SessionStore) -> Self {
        SessionManager { inner: Arc::new(SessionManager::defaults(key, store)) }
    }

    /// Build from configuration: `app.key`, `session.cookie`, `session.lifetime`.
    ///
    /// The store keeps its own lifetime — build it with
    /// [`SessionManager::lifetime_from_config`] so the cookie and the stored
    /// session expire together.
    pub fn from_config(config: &Config, store: impl SessionStore) -> Result<Self> {
        let mut inner = SessionManager::defaults(&AppKey::from_config(config)?, store);
        inner.cookie = config.string("session.cookie", DEFAULT_COOKIE);
        inner.lifetime = SessionManager::lifetime_from_config(config);
        inner.path = config.string("session.path", "/");
        // Default on in production, where the site is expected to be HTTPS.
        inner.secure = config.bool("session.secure", config.is_production());

        Ok(SessionManager { inner: Arc::new(inner) })
    }

    fn defaults(key: &AppKey, store: impl SessionStore) -> Inner {
        let lifetime = store.lifetime();
        Inner {
            store: Arc::new(store),
            cookie_key: key.derive("session-cookie"),
            cookie: DEFAULT_COOKIE.to_string(),
            lifetime,
            path: "/".to_string(),
            // `SameSite=Lax` already blocks the cross-site POST that CSRF
            // depends on, so the cookie is only marked `Secure` when the
            // application is actually served over HTTPS — otherwise local
            // development over http:// would silently lose its session.
            secure: false,
            same_site: SameSite::Lax,
        }
    }

    /// `session.lifetime`, in minutes, as Laravel writes it.
    pub fn lifetime_from_config(config: &Config) -> Duration {
        match config.int("session.lifetime", -1) {
            minutes if minutes > 0 => Duration::from_secs(minutes as u64 * 60),
            _ => DEFAULT_LIFETIME,
        }
    }

    pub fn cookie_name(mut self, name: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).cookie = name.into();
        self
    }

    pub fn lifetime(mut self, lifetime: Duration) -> Self {
        Arc::make_mut(&mut self.inner).lifetime = lifetime;
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.inner).path = path.into();
        self
    }

    /// Mark the cookie `Secure`, so browsers only send it over HTTPS.
    pub fn secure(mut self, secure: bool) -> Self {
        Arc::make_mut(&mut self.inner).secure = secure;
        self
    }

    pub fn same_site(mut self, same_site: SameSite) -> Self {
        Arc::make_mut(&mut self.inner).same_site = same_site;
        self
    }

    pub fn store(&self) -> &SharedStore {
        &self.inner.store
    }

    /// The signed cookie value for a session id: `<id>.<mac>`.
    ///
    /// Signing does not hide the id — it is `HttpOnly` and travels over TLS —
    /// it stops the *store* being probed. Without a MAC, anybody could send
    /// millions of guessed ids and make the server hit the filesystem for each
    /// one; with it, a forged cookie is rejected before the store is touched.
    fn sign(&self, id: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.inner.cookie_key).expect("HMAC accepts any key length");
        mac.update(id.as_bytes());
        format!("{id}.{}", base64::encode_url(&mac.finalize().into_bytes()))
    }

    /// Recover the id from a cookie, or `None` if it was not signed by us.
    fn unsign(&self, value: &str) -> Option<String> {
        let (id, signature) = value.rsplit_once('.')?;
        if !is_valid_id(id) {
            return None;
        }

        let expected = self.sign(id);
        let expected = expected.rsplit_once('.')?.1;
        constant_time_eq(expected.as_bytes(), signature.as_bytes()).then(|| id.to_string())
    }

    fn cookie_for(&self, id: &str) -> Cookie {
        Cookie::new(self.inner.cookie.clone(), self.sign(id))
            .path(self.inner.path.clone())
            .max_age(self.inner.lifetime)
            .http_only(true)
            .secure(self.inner.secure)
            .same_site(self.inner.same_site)
    }
}

impl Clone for Inner {
    fn clone(&self) -> Self {
        Inner {
            store: Arc::clone(&self.store),
            cookie_key: self.cookie_key,
            cookie: self.cookie.clone(),
            lifetime: self.lifetime,
            path: self.path.clone(),
            secure: self.secure,
            same_site: self.same_site,
        }
    }
}

impl Middleware for SessionManager {
    fn handle(&self, mut request: Request, next: Next) -> BoxFuture<Response> {
        let manager = self.clone();

        Box::pin(async move {
            let presented = request
                .cookie(&manager.inner.cookie)
                .and_then(|value| manager.unsign(&value));

            let (session, existed) = match &presented {
                Some(id) => match manager.inner.store.read(id).await {
                    Ok(Some(session)) => (session, true),
                    Ok(None) => (Session::new(), false),
                    Err(error) => {
                        // A store that is down must not take the site down with
                        // it: the visitor gets a fresh, empty session.
                        rustlavel_core::error!("could not read the session: {error}");
                        (Session::new(), false)
                    }
                },
                None => (Session::new(), false),
            };

            let loaded_id = session.id().to_string();
            let handle = SessionHandle::new(session);

            // Read before the request is moved into the pipeline.
            let method = request.method();
            let wants_json = request.wants_json();
            let target = request.target().to_string();

            request.extend(handle.clone());
            // Registered a second time behind the trait, so validation and the
            // view layer can leave and read a flash without depending on this
            // crate. See `rustlavel_http::flash`.
            let flash: std::sync::Arc<dyn rustlavel_http::Flash> = std::sync::Arc::new(handle.clone());
            request.extend(flash);

            let response = next.run(request).await;

            // Record where the visitor is, so a form that fails validation
            // later knows where "back" is.
            //
            // After the handler, not before, and only for a session that is
            // already being written. Recording unconditionally would start a
            // session — and set a cookie — for everybody who ever loaded a
            // page, which is both a privacy imposition and the end of caching
            // anonymous responses. Running afterwards is what makes the common
            // case work anyway: a page with a form has put a CSRF token in the
            // session by now, so the request that needs this has a session by
            // the time we ask. A page that uses no session at all falls back to
            // the `Referer`.
            //
            // Only a page view counts. A POST is not somewhere to return to,
            // and neither is a redirect or an answer to a request for JSON.
            if matches!(method, Method::Get)
                && !wants_json
                && (200..300).contains(&response.status.code())
                && handle.lock().is_dirty()
            {
                handle.put(rustlavel_http::flash::PREVIOUS_URL_KEY, target.clone());
            }

            // Age the flash and take a copy, then drop the lock: the guard must
            // not be alive across the store's `.await`.
            let (session, dirty) = {
                let mut session = handle.lock();
                session.age_flash();
                (session.clone(), session.is_dirty())
            };

            // A request that changed nothing writes nothing and sets no cookie.
            //
            // `!existed` alone was not enough, and the gap was a real one: for a
            // signed-in visitor `existed` is always true, so *every* response
            // rewrote the session and re-sent the cookie — including the CSS,
            // JavaScript and font requests, which reach here because static
            // files are the router's fallback and the fallback still runs the
            // global middleware.
            //
            // That made signing in a race. `Guard::login` rotates the id and
            // destroys the old record; the asset requests the browser fired
            // alongside the form post are still holding the *old* id, and on
            // the way out each one wrote that old session back — resurrecting
            // the record the rotation had just deleted — and handed the browser
            // a cookie pointing at it. Whichever response landed last decided
            // which session the visitor kept. When an asset won, the visitor
            // was returned to a pre-login session and the next protected
            // request answered 401: signed out seconds after signing in, and
            // only sometimes, which is the worst way for a bug to behave.
            //
            // A crawler still leaves nothing behind, which is what the original
            // condition was for.
            if !dirty && session.id() == loaded_id {
                return response;
            }

            // `regenerate` gave the session a new id; the old record is now a
            // valid credential nobody should still be able to present.
            if existed && session.id() != loaded_id
                && let Err(error) = manager.inner.store.destroy(&loaded_id).await
            {
                rustlavel_core::error!("could not destroy the previous session: {error}");
            }

            if let Err(error) = manager.inner.store.write(&session).await {
                rustlavel_core::error!("could not write the session: {error}");
                return response;
            }

            response.with_cookie(manager.cookie_for(session.id()))
        })
    }
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("cookie", &self.inner.cookie)
            .field("lifetime", &self.inner.lifetime)
            .field("secure", &self.inner.secure)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use rustlavel_http::{Router, TestClient};

    fn key() -> AppKey {
        AppKey::from_bytes([3u8; 32])
    }

    fn manager() -> SessionManager {
        SessionManager::new(&key(), MemoryStore::new())
    }


    /// A response that changed nothing must not hand back a session cookie.
    ///
    /// This is the whole of the sign-in race. Static files are the router's
    /// fallback and the fallback runs the global middleware, so the CSS and
    /// JavaScript requests a browser fires alongside a sign-in reach here
    /// holding the *old* id. While every such response re-sent a cookie, one of
    /// them could land after the redirect and put the visitor back on the
    /// session the sign-in had just replaced — signed out seconds later, and
    /// only sometimes.
    #[tokio::test]
    async fn a_request_that_changes_nothing_sets_no_cookie() {
        let mut router = Router::new();
        router.middleware(manager());
        router.get("/touches", |req: Request| async move {
            req.session().put("something", Json::from("changed"));
            "wrote"
        });
        router.get("/reads-nothing", |_req: Request| async { "untouched" });

        let client = TestClient::new(router);

        // A first request that does write, to get a session at all.
        let first = client.get("/touches").await;
        let cookie = cookie_of(&first);

        // And now one that touches nothing, carrying that session.
        let second = client
            .send(Request::new(Method::Get, "/reads-nothing").with_header("cookie", cookie))
            .await;
        assert!(
            second.header("set-cookie").is_none(),
            "a response that changed nothing re-sent the session cookie, which is what let an \
             asset request hand back an id a concurrent sign-in had already replaced"
        );
    }

    /// And a session that did change still writes, or nothing would persist.
    #[tokio::test]
    async fn a_request_that_changes_the_session_still_sets_a_cookie() {
        let mut router = Router::new();
        router.middleware(manager());
        router.get("/touches", |req: Request| async move {
            req.session().put("n", Json::from(1.0));
            "wrote"
        });

        let client = TestClient::new(router);
        let response = client.get("/touches").await;
        assert!(response.header("set-cookie").is_some(), "a changed session was not persisted");
    }

    /// Pull the session cookie out of a response, ready to send back.
    fn cookie_of(response: &rustlavel_http::TestResponse) -> String {
        let header = response
            .header("set-cookie")
            .unwrap_or_else(|| panic!("no set-cookie header on the response"));
        header.split(';').next().unwrap().to_string()
    }

    fn counting_router(manager: SessionManager) -> Router {
        let mut router = Router::new();
        router.middleware(manager);
        router.get("/count", |request: Request| async move {
            let visits = request.session().get("visits").and_then(|v| v.as_i64()).unwrap_or(0) + 1;
            request.session().put("visits", visits);
            format!("{visits}")
        });
        router.get("/quiet", |_request: Request| async { "nothing to see" });
        router.get("/flash", |request: Request| async move {
            request.session().flash("status", "Saved");
            "flashed"
        });
        router.get("/status", |request: Request| async move {
            request.session().get_string("status").unwrap_or_else(|| "none".into())
        });
        router.get("/rotate", |request: Request| async move {
            request.session().put("kept", "yes");
            request.session().regenerate()
        });
        router
    }

    #[tokio::test]
    async fn a_session_survives_between_requests_through_the_cookie() {
        let client = TestClient::new(counting_router(manager()));

        let first = client.get("/count").await.assert_ok();
        assert_eq!(first.body(), "1");
        let cookie = cookie_of(&first);
        assert!(cookie.starts_with(DEFAULT_COOKIE));

        let second = client
            .send(Request::new(Method::Get, "/count").with_header("cookie", cookie.clone()))
            .await;
        assert_eq!(second.body(), "2");

        let third = client
            .send(Request::new(Method::Get, "/count").with_header("cookie", cookie))
            .await;
        assert_eq!(third.body(), "3");
    }

    #[tokio::test]
    async fn the_session_cookie_is_http_only_and_scoped() {
        let response = TestClient::new(counting_router(manager())).get("/count").await;
        let header = response.header("set-cookie").unwrap();

        assert!(header.contains("HttpOnly"), "header was {header}");
        assert!(header.contains("SameSite=Lax"));
        assert!(header.contains("Path=/"));
        assert!(header.contains("Max-Age=7200"));
        assert!(!header.contains("Secure"), "a plain-http development cookie is not Secure");
    }

    #[tokio::test]
    async fn a_request_that_never_touches_the_session_gets_no_cookie() {
        let response = TestClient::new(counting_router(manager())).get("/quiet").await.assert_ok();

        assert_eq!(
            response.header("set-cookie"),
            None,
            "nothing was stored, so there is nothing to hand the client"
        );
    }

    #[tokio::test]
    async fn a_forged_or_unsigned_cookie_starts_a_fresh_session() {
        let client = TestClient::new(counting_router(manager()));

        let real = client.get("/count").await;
        let cookie = cookie_of(&real);

        // Same id, signature replaced.
        let (name_and_id, _) = cookie.rsplit_once('.').unwrap();
        let forged = format!("{name_and_id}.{}", base64::encode_url(&[0u8; 32]));
        let response = client
            .send(Request::new(Method::Get, "/count").with_header("cookie", forged))
            .await;
        assert_eq!(response.body(), "1", "a forged cookie must not open the real session");

        // An id-shaped value with no signature at all.
        let response = client
            .send(Request::new(Method::Get, "/count").with_header(
                "cookie",
                format!("{DEFAULT_COOKIE}={}", Session::new_id()),
            ))
            .await;
        assert_eq!(response.body(), "1");
    }

    #[test]
    fn a_cookie_signed_with_another_key_is_refused() {
        let mine = SessionManager::new(&key(), MemoryStore::new());
        let theirs = SessionManager::new(&AppKey::from_bytes([4u8; 32]), MemoryStore::new());
        let id = Session::new_id();

        assert_eq!(mine.unsign(&mine.sign(&id)).as_deref(), Some(id.as_str()));
        assert!(mine.unsign(&theirs.sign(&id)).is_none());
        // And a value that is not even shaped like a signed cookie.
        assert!(mine.unsign("no-dot-here").is_none());
        assert!(mine.unsign(&format!("not-an-id.{}", base64::encode_url(&[0u8; 32]))).is_none());
    }

    #[tokio::test]
    async fn flashed_data_is_readable_on_the_next_request_and_gone_after_it() {
        let client = TestClient::new(counting_router(manager()));

        let flashed = client.get("/flash").await;
        let cookie = cookie_of(&flashed);

        let next = client
            .send(Request::new(Method::Get, "/status").with_header("cookie", cookie.clone()))
            .await;
        assert_eq!(next.body(), "Saved");

        let after = client
            .send(Request::new(Method::Get, "/status").with_header("cookie", cookie))
            .await;
        assert_eq!(after.body(), "none");
    }

    #[tokio::test]
    async fn regenerating_issues_a_new_cookie_and_drops_the_old_record() {
        let manager = manager();
        let store = Arc::clone(manager.store());
        let client = TestClient::new(counting_router(manager));

        let first = client.get("/count").await;
        let old_cookie = cookie_of(&first);
        let old_id = old_cookie.split_once('=').unwrap().1.rsplit_once('.').unwrap().0.to_string();
        assert!(store.read(&old_id).await.unwrap().is_some());

        let rotated = client
            .send(Request::new(Method::Get, "/rotate").with_header("cookie", old_cookie.clone()))
            .await;
        let new_id = rotated.body();
        let new_cookie = cookie_of(&rotated);

        assert_ne!(new_id, old_id);
        assert!(store.read(&old_id).await.unwrap().is_none(), "the old id must stop working");

        let carried = store.read(&new_id).await.unwrap().expect("the new id holds the session");
        assert_eq!(carried.get_string("kept").as_deref(), Some("yes"));
        assert_ne!(new_cookie, old_cookie);
    }

    #[tokio::test]
    async fn an_expired_stored_session_starts_over() {
        let manager = SessionManager::new(&key(), MemoryStore::with_lifetime(Duration::ZERO));
        let client = TestClient::new(counting_router(manager));

        let first = client.get("/count").await;
        let cookie = cookie_of(&first);

        let second = client
            .send(Request::new(Method::Get, "/count").with_header("cookie", cookie))
            .await;
        assert_eq!(second.body(), "1", "the stored session had already expired");
    }

    #[test]
    fn configuration_drives_the_cookie_name_lifetime_and_secure_flag() {
        let config = Config::new();
        config.set("app.key", AppKey::generate());
        config.set("session.cookie", "my_app_session");
        config.set("session.lifetime", 30);
        config.set("app.env", "production");

        let manager = SessionManager::from_config(&config, MemoryStore::new()).unwrap();

        assert_eq!(manager.inner.cookie, "my_app_session");
        assert_eq!(manager.inner.lifetime, Duration::from_secs(30 * 60));
        assert!(manager.inner.secure, "production cookies default to Secure");
        assert_eq!(SessionManager::lifetime_from_config(&Config::new()), DEFAULT_LIFETIME);
    }

    #[test]
    fn builders_override_configuration() {
        let manager = manager()
            .cookie_name("custom")
            .lifetime(Duration::from_secs(60))
            .path("/app")
            .secure(true)
            .same_site(SameSite::Strict);

        assert_eq!(manager.inner.cookie, "custom");
        assert_eq!(manager.inner.path, "/app");
        assert!(manager.inner.secure);

        let header = manager.cookie_for(&Session::new_id()).to_header();
        assert!(header.contains("SameSite=Strict"));
        assert!(header.contains("Secure"));
        assert!(header.starts_with("custom="));
    }

    #[tokio::test]
    async fn a_handler_without_the_middleware_says_what_is_missing() {
        let mut router = Router::new();
        router.get("/", |request: Request| async move {
            assert!(request.try_session().is_none());
            request.session().id()
        });

        // The router turns the panic into a 500 rather than aborting the test.
        let response = TestClient::new(router).get("/").await;
        assert_eq!(response.status(), 500);
    }
}
