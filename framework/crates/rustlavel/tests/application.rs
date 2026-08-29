//! End-to-end tests for an application built on the framework.

use rustlavel::test_prelude::*;
use rustlavel::error_page;

fn app() -> App {
    App::bare().routes(|r| {
        r.get("/", |_req: Request| async { "<h1>Home</h1>" });
        r.get("/users/{id}", |req: Request| async move {
            match req.param_as::<u32>("id") {
                Some(id) => Response::json(Json::object([("id", id.into())])),
                None => Response::not_found(),
            }
        });
        r.post("/users", |mut req: Request| async move {
            match req.input("name") {
                Some(name) if !name.is_empty() => {
                    (201, Json::object([("name", Json::from(name))])).into_response()
                }
                _ => Response::new(Status::UNPROCESSABLE)
                    .with_json(Json::object([("error", "name is required".into())])),
            }
        });
        r.get("/boom", |_req: Request| async {
            let missing: Option<u8> = None;
            #[allow(clippy::unnecessary_literal_unwrap)]
            missing.expect("this handler panics on purpose");
            Response::ok()
        });

        r.group("/admin", |r| {
            r.middleware(|req: Request, next: Next| async move {
                if req.header("x-token") == Some("secret") {
                    next.run(req).await
                } else {
                    Response::new(Status::UNAUTHORIZED).with_text("nope")
                }
            });
            r.get("/dashboard", |_req: Request| async { "admin area" });
        });
    })
}

#[tokio::test]
async fn serves_html_json_and_route_parameters() {
    let client = app().test_client();

    client.get("/").await.assert_ok().assert_see("Home");
    client.get("/users/42").await.assert_ok().assert_json("id", 42);
    client.get("/users/abc").await.assert_not_found();
}

#[tokio::test]
async fn validates_a_posted_form() {
    let client = app().test_client();

    client.post("/users", &[("name", "Ada")]).await.assert_status(201).assert_json("name", "Ada");
    client.post("/users", &[]).await.assert_status(422).assert_json("error", "name is required");
}

#[tokio::test]
async fn group_middleware_guards_its_routes() {
    let client = app().test_client();

    client.get("/admin/dashboard").await.assert_status(401);
    client
        .send(Request::new(Method::Get, "/admin/dashboard").with_header("x-token", "secret"))
        .await
        .assert_ok()
        .assert_see("admin area");
}

#[tokio::test]
async fn a_panicking_handler_becomes_an_error_page_rather_than_a_crash() {
    error_page::set_debug(true);
    let client = app().test_client();

    let response = client.get("/boom").await.assert_status(500);
    assert!(response.body().contains("this handler panics on purpose"));

    // The rest of the application keeps serving.
    client.get("/").await.assert_ok();
    error_page::set_debug(false);
}

#[tokio::test]
async fn unknown_paths_and_methods_are_reported_distinctly() {
    let client = app().test_client();

    client.get("/nowhere").await.assert_not_found();
    client.delete("/users").await.assert_status(405).assert_header("allow", "POST");
}

/// Validation reaching a handler through `?`, which is what the generalised
/// `IntoResponse for Result` in the HTTP crate exists to allow.
#[cfg(feature = "validation")]
mod validation {
    use super::*;
    use rustlavel::validation::validate;

    fn app() -> App {
        App::bare().routes(|r| {
            r.post("/register", |mut req: Request| async move {
                let data = validate(
                    &mut req,
                    &[("email", "required|email"), ("age", "required|integer|min:18")],
                )
                .await?;

                Ok::<_, rustlavel::validation::Errors>(
                    (201, Json::object([("email", Json::from(data.string("email").unwrap_or_default()))]))
                        .into_response(),
                )
            });
        })
    }

    #[tokio::test]
    async fn valid_input_reaches_the_handler() {
        app()
            .test_client()
            .post_json(
                "/register",
                Json::object([("email", "ada@example.com".into()), ("age", 36.into())]),
            )
            .await
            .assert_status(201)
            .assert_json("email", "ada@example.com");
    }

    #[tokio::test]
    async fn invalid_input_becomes_a_422_before_the_handler_body_runs() {
        let response = app()
            .test_client()
            .post_json("/register", Json::object([("email", "not-an-email".into()), ("age", 12.into())]))
            .await
            .assert_status(422);

        let body = response.json();
        assert!(body.get("errors.email").is_some(), "email should be rejected: {}", response.body());
        assert!(body.get("errors.age").is_some(), "age should be rejected: {}", response.body());
    }
}
