//! The tests the starter kit ships with.
//!
//! They cover the two things that must stay true however the pages are
//! restyled: a signed-out visitor is sent to the sign-in page rather than
//! shown anything, and the pages that take a secret refuse a request with no
//! CSRF token. Both are checked through the router, so they exercise the real
//! middleware stack rather than a controller in isolation.
//!
//! Anything below this reaches the database, so it lives beside your own
//! tests rather than here — the kit cannot know what yours is called.

use rustlavel::test_prelude::*;

/// The application, minus the parts that need a database or a mail server.
///
/// Enough to prove the guard and the CSRF check are wired; a handler that
/// needs a row is left to a test that has one.
fn app() -> App {
    App::bare()
        .middleware(SessionManager::new(
            &rustlavel::auth::AppKey::from_bytes([7u8; 32]),
            rustlavel::auth::MemoryStore::new(),
        ))
        .middleware(Csrf::new())
        .routes({{crate_name}}::routes::auth::routes)
        .routes({{crate_name}}::routes::web::routes)
}

#[rustlavel::test]
async fn the_sign_in_page_is_public() {
    app().test_client().get("/login").await.assert_ok().assert_see("Sign in");
}

#[rustlavel::test]
async fn a_signed_out_visitor_is_sent_to_the_sign_in_page() {
    let client = app().test_client();
    for path in ["/", "/dashboard", "/profile", "/settings/security", "/admin/users"] {
        let response = client.get(path).await;
        assert!(
            (300..400).contains(&response.status()),
            "{path} answered {} for a signed-out visitor",
            response.status()
        );
    }
}

#[rustlavel::test]
async fn a_form_post_without_a_csrf_token_is_refused() {
    // 419 rather than 403, which is the status Laravel uses for an expired
    // token and what the error page is written for.
    let client = app().test_client();
    let response = client.post("/login", &[("email", "a@example.com"), ("password", "x")]).await;
    assert_eq!(response.status(), 419, "a login without a token must not reach the handler");
}

#[rustlavel::test]
async fn the_sign_in_form_carries_a_token_to_send_back() {
    let body = app().test_client().get("/login").await.body();
    assert!(body.contains(r#"name="_token""#), "the form has no CSRF field");
}
