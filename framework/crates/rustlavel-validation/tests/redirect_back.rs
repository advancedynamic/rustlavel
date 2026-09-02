//! A failed form, all the way round.
//!
//! Unit tests can show that the messages are built. What they cannot show is
//! the part that actually matters to a person: submit a bad form, land back on
//! it, and find the errors displayed and the boxes still filled. That takes the
//! session middleware, a redirect and a second request, so it lives here.

use rustlavel_auth::{AppKey, Csrf, MemoryStore, SessionManager};
use rustlavel_http::{Method, Request, Response, Router, TestClient};
use rustlavel_validation::validate;

/// A two-page application: a form, and something that validates a submission.
///
/// The CSRF middleware is here because a real form has it, and because it is
/// what puts a token in the session — which is how the form page comes to have
/// a session for the redirect target to be recorded in. A page that touches no
/// session at all is covered separately, below.
fn client() -> TestClient {
    let mut router = Router::new();
    router.middleware(SessionManager::new(&AppKey::from_bytes([3u8; 32]), MemoryStore::new()));
    router.middleware(Csrf::new().except("/subscribe"));

    // The form redraws itself from whatever the last request left behind.
    router.get("/subscribe", |req: Request| async move {
        let errors = req.errors();
        let body = format!(
            "<form method=post>\
             <p class=error>{}</p>\
             <input name=name value=\"{}\">\
             <input name=email value=\"{}\">\
             <input name=password type=password value=\"{}\">\
             </form>",
            errors.get("email.0").and_then(rustlavel_core::Json::as_str).unwrap_or(""),
            req.old_field("name"),
            req.old_field("email"),
            req.old_field("password"),
        );
        Response::html(body)
    });

    router.post("/subscribe", |mut req: Request| async move {
        match validate(
            &mut req,
            &[("name", "required|max:60"), ("email", "required|email"), ("password", "required|min:8")],
        )
        .await
        {
            Ok(data) => Response::text(format!("welcome {}", data.string("name").unwrap_or_default())),
            Err(errors) => rustlavel_http::IntoResponse::into_response(errors),
        }
    });

    TestClient::new(router)
}

fn submit(fields: &[(&str, &str)]) -> Request {
    Request::new(Method::Post, "/subscribe").with_form(fields)
}

#[tokio::test]
async fn a_bad_submission_sends_the_browser_back_to_the_form() {
    let client = client();
    client.get("/subscribe").await.assert_ok();

    let response = client
        .send(submit(&[("name", "Ada"), ("email", "not-an-address"), ("password", "short")]))
        .await;

    // 303 rather than 302, so the browser follows it with a GET and a reload
    // does not re-submit the form.
    assert_eq!(response.status(), 303);
    assert_eq!(response.header("location"), Some("/subscribe"));
    assert_eq!(response.body(), "", "the answer is the redirect, not a page");
}

#[tokio::test]
async fn the_form_comes_back_with_the_messages_and_what_was_typed() {
    let client = client();
    client.get("/subscribe").await;
    client.send(submit(&[("name", "Ada"), ("email", "not-an-address"), ("password", "hunter22")])).await;

    let page = client.get("/subscribe").await;
    let body = page.body();

    assert!(body.contains("must be a valid email address"), "no message rendered: {body}");
    assert!(body.contains(r#"value="Ada""#), "the name was not given back: {body}");
    assert!(body.contains(r#"value="not-an-address""#), "the address was not given back: {body}");
}

#[tokio::test]
async fn the_password_is_never_handed_back() {
    // Re-filling a password field means putting the password into HTML that
    // ends up in caches, in history and in screenshots.
    let client = client();
    client.get("/subscribe").await;
    client.send(submit(&[("name", "Ada"), ("email", "no"), ("password", "hunter22")])).await;

    let body = client.get("/subscribe").await.body();
    assert!(!body.contains("hunter22"), "the password came back in the page: {body}");
}

#[tokio::test]
async fn the_messages_last_exactly_one_request() {
    let client = client();
    client.get("/subscribe").await;
    client.send(submit(&[("name", ""), ("email", "no"), ("password", "x")])).await;

    assert!(client.get("/subscribe").await.body().contains("valid email address"));
    // Reloading the form again must not show a complaint about a submission
    // two clicks ago.
    let second = client.get("/subscribe").await.body();
    assert!(!second.contains("valid email address"), "the errors outlived their request: {second}");
    assert!(!second.contains(r#"value="no""#), "so did the old input");
}

#[tokio::test]
async fn a_good_submission_reaches_the_handler() {
    let client = client();
    client.get("/subscribe").await;
    let response = client
        .send(submit(&[("name", "Ada"), ("email", "ada@example.com"), ("password", "hunter22")]))
        .await;

    let response = response.assert_ok();
    assert_eq!(response.body(), "welcome Ada");
    // And nothing was left behind for the next page to display.
    assert!(!client.get("/subscribe").await.body().contains("class=error>The"));
}

#[tokio::test]
async fn a_json_client_still_gets_its_422() {
    // The browser half must not change what an API client sees.
    let client = client();
    let response = client
        .send(submit(&[("email", "no")]).with_header("accept", "application/json"))
        .await;

    let response = response.assert_status(422);
    let body = response.json();
    assert!(body.get("message").is_some(), "Laravel's envelope: {}", response.body());
    assert!(body.get("errors.email").is_some());
    assert_eq!(response.header("location"), None, "an API client is not redirected");
}

#[tokio::test]
async fn without_a_session_it_falls_back_to_text_rather_than_breaking() {
    // An application with no session middleware still gets an answer, just a
    // plainer one.
    let mut router = Router::new();
    router.post("/subscribe", |mut req: Request| async move {
        match validate(&mut req, &[("email", "required|email")]).await {
            Ok(_) => Response::text("ok"),
            Err(errors) => rustlavel_http::IntoResponse::into_response(errors),
        }
    });
    let response = TestClient::new(router).send(submit(&[("email", "no")])).await;

    let response = response.assert_status(422);
    assert!(response.body().contains("email"));
    assert_eq!(response.header("location"), None);
}

#[tokio::test]
async fn back_is_the_page_the_form_was_on_not_wherever_the_referer_says() {
    let client = client();
    client.get("/subscribe").await;

    let hostile = submit(&[("email", "no")]).with_header("referer", "https://evil.example/login");
    let response = client.send(hostile).await;

    assert_eq!(response.header("location"), Some("/subscribe"));
}

#[tokio::test]
async fn a_page_that_touches_no_session_falls_back_to_the_referer() {
    // The previous page is recorded only for a visitor who already has a
    // session, so that a cookie is never set on somebody who was reading an
    // anonymous page. A form with no CSRF token and no login is that case, and
    // it relies on the header the browser sends instead.
    let mut router = Router::new();
    router.middleware(SessionManager::new(&AppKey::from_bytes([4u8; 32]), MemoryStore::new()));
    router.get("/plain", |_req: Request| async { Response::html("<form method=post></form>") });
    router.post("/plain", |mut req: Request| async move {
        match validate(&mut req, &[("email", "required|email")]).await {
            Ok(_) => Response::text("ok"),
            Err(errors) => rustlavel_http::IntoResponse::into_response(errors),
        }
    });
    let client = TestClient::new(router);
    client.get("/plain").await;

    let with_referer = Request::new(Method::Post, "/plain")
        .with_form(&[("email", "no")])
        .with_header("referer", "/plain");
    assert_eq!(client.send(with_referer).await.header("location"), Some("/plain"));

    // And with neither, it goes to the root rather than guessing.
    let bare = Request::new(Method::Post, "/plain").with_form(&[("email", "no")]);
    assert_eq!(client.send(bare).await.header("location"), Some("/"));
}

#[tokio::test]
async fn it_works_behind_the_csrf_check() {
    // The two middleware have to compose: CSRF reads the session, validation
    // writes to it, and the token has to survive the redirect.
    let mut router = Router::new();
    router.middleware(SessionManager::new(&AppKey::from_bytes([9u8; 32]), MemoryStore::new()));
    router.middleware(Csrf::new());
    router.get("/subscribe", |req: Request| async move {
        Response::html(format!(
            "{}<input name=email value=\"{}\">",
            rustlavel_auth::csrf::field(&req),
            req.old_field("email")
        ))
    });
    router.post("/subscribe", |mut req: Request| async move {
        match validate(&mut req, &[("email", "required|email")]).await {
            Ok(_) => Response::text("ok"),
            Err(errors) => rustlavel_http::IntoResponse::into_response(errors),
        }
    });
    let client = TestClient::new(router);

    let page = client.get("/subscribe").await.body();
    let token = page
        .split(r#"value=""#)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a CSRF token in the form")
        .to_string();

    let response = client.send(submit(&[("_token", &token), ("email", "no")])).await;
    assert_eq!(response.status(), 303, "the token was accepted and validation ran");
    assert!(client.get("/subscribe").await.body().contains(r#"value="no""#));
}
