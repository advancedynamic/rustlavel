use rustlavel::test_prelude::*;

fn app() -> App {
    App::bare().routes(blog::routes::web::routes)
}

#[tokio::test]
async fn the_home_page_renders() {
    app().test_client().get("/").await.assert_ok();
}
