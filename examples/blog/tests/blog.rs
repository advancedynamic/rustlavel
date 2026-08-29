//! Tests for the example.
//!
//! They dispatch through the router without opening a socket, so the whole file
//! runs in milliseconds. Only the tests that need real rows ask for a database.

use blog::routes;
use rustlavel::test_prelude::*;
use rustlavel::views::Engine;

/// The application, with views but no database.
fn app() -> App {
    App::bare()
        .views(Engine::new("resources/views"))
        .routes(routes::web::routes)
}

/// The application with a database, or `None` when DATABASE_URL is unset.
async fn app_with_database() -> Option<App> {
    let url = std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty())?;
    let db = Database::connect(&url).await.ok()?;

    // A table of its own, so the example's tests cannot disturb a real blog.
    db.run("drop table if exists posts cascade").await.ok()?;
    Schema::new(&db)
        .create("posts", |t| {
            t.id();
            t.string("title");
            t.text("body");
            t.boolean("published").default_bool(false);
        })
        .await
        .ok()?;

    Some(app().state(db))
}

#[tokio::test]
async fn the_home_page_renders() {
    app().test_client().get("/").await.assert_ok().assert_see("Read the posts");
}

#[tokio::test]
async fn the_write_form_is_reachable() {
    app()
        .test_client()
        .get("/posts/new")
        .await
        .assert_ok()
        .assert_see("Write a post")
        .assert_see("</form>");
}

#[tokio::test]
async fn an_unknown_page_is_a_404() {
    app().test_client().get("/nowhere").await.assert_not_found();
}

#[tokio::test]
async fn a_short_post_is_rejected_before_it_reaches_the_database() {
    // No database is registered, and none is needed: validation runs first.
    // A browser form gets a plain 422; a JSON client gets the error object.
    app()
        .test_client()
        .post("/posts", &[("title", "Hi"), ("body", "too short")])
        .await
        .assert_status(422)
        .assert_see("at least 10 characters");
}

#[tokio::test]
async fn a_json_client_gets_the_errors_as_json() {
    let response = app()
        .test_client()
        .post_json("/posts", Json::object([("body", "A body long enough to pass.".into())]))
        .await
        .assert_status(422);

    response.assert_json("errors.title.0", "The title field is required.");
}

#[tokio::test]
async fn a_post_can_be_written_read_and_listed() {
    let Some(app) = app_with_database().await else {
        eprintln!("skipped: set DATABASE_URL to run the database-backed tests");
        return;
    };
    let client = app.test_client();

    client.get("/posts").await.assert_ok().assert_see("Nothing published yet");

    let created = client
        .post(
            "/posts",
            &[("title", "Hello from Rustlavel"), ("body", "A body that is comfortably long enough.")],
        )
        .await
        .assert_status(303);

    let location = created.header("location").expect("a redirect to the new post").to_string();

    client
        .get(&location)
        .await
        .assert_ok()
        .assert_see("Hello from Rustlavel")
        .assert_see("comfortably long enough");

    client.get("/posts").await.assert_ok().assert_see("Hello from Rustlavel");
}

#[tokio::test]
async fn a_post_title_cannot_inject_markup() {
    let Some(app) = app_with_database().await else {
        eprintln!("skipped: set DATABASE_URL to run the database-backed tests");
        return;
    };
    let client = app.test_client();

    let created = client
        .post(
            "/posts",
            &[
                ("title", "<script>alert(1)</script>"),
                ("body", "The template escapes this without being asked."),
            ],
        )
        .await
        .assert_status(303);

    client
        .get(created.header("location").unwrap())
        .await
        .assert_ok()
        .assert_dont_see("<script>alert(1)</script>")
        .assert_see("&lt;script&gt;");
}

#[tokio::test]
async fn asking_for_a_post_that_is_not_there_is_a_404() {
    let Some(app) = app_with_database().await else {
        eprintln!("skipped: set DATABASE_URL to run the database-backed tests");
        return;
    };

    app.test_client().get("/posts/999999").await.assert_not_found();
}
