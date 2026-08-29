//! Rustlavel's entry in the benchmark. See `benchmarks/CONTRACT.md`.
//!
//! Written as an ordinary application rather than tuned for the measurement:
//! the point is what somebody gets by using the framework, not what it can be
//! made to do by an author who knows where the fast paths are.

use rustlavel::prelude::*;
use rustlavel::views::{Context as ViewContext, Engine};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // The framework reads its port from `SERVER_PORT`; the harness sets
    // `PORT`, so translate rather than teaching the harness a special case.
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    // SAFETY: single-threaded, before the runtime serves anything.
    unsafe { std::env::set_var("SERVER_PORT", &port) };
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");

    let mut config = rustlavel::db::DatabaseConfig::from_url(&url)?;
    config.max_connections = 16;
    let db = Database::with_config(config).await?;

    let views = Arc::new(Engine::new(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/views")));

    App::bare()
        .state(db)
        .state(views)
        .routes(routes)
        .serve()
        .await
}

fn routes(r: &mut Router) {
    r.get("/plaintext", |_req: Request| async move {
        Response::text("Hello, World!")
    });

    r.get("/json", |_req: Request| async move {
        Response::json(Json::object([("message", Json::from("Hello, World!"))]))
    });

    r.get("/users/{id}/posts/{slug}", |req: Request| async move {
        let id: i64 = req.param("id").and_then(|v| v.parse().ok()).unwrap_or(0);
        let slug = req.param("slug").unwrap_or_default();
        Response::json(Json::object([
            ("id", Json::from(id as f64)),
            ("slug", Json::from(slug)),
        ]))
    });

    // Five middlewares, as the contract requires. A group rather than five
    // global ones, so the other endpoints are not measured through them.
    r.group("", |group| {
        for index in 1..=5u8 {
            group.middleware(move |req: Request, next: Next| async move {
                next.run(req).await.with_header(&format!("x-bench-{index}"), "ok")
            });
        }
        group.get("/middleware", |_req: Request| async move {
            Response::json(Json::object([("depth", Json::from(5.0))]))
        });
    });

    r.get("/json-big", |_req: Request| async move {
        let rows: Vec<Json> = (1..=100)
            .map(|id| {
                Json::object([
                    ("id", Json::from(id as f64)),
                    ("name", Json::from(format!("User {id}"))),
                    ("email", Json::from(format!("user{id}@example.test"))),
                    ("active", Json::Bool(id % 2 == 0)),
                    ("score", Json::from(id as f64 * 1.5)),
                ])
            })
            .collect();
        Response::json(Json::Array(rows))
    });

    r.get("/db/user/{id}", |req: Request| async move {
        let db = req.state::<Database>().expect("the database");
        let id: i64 = req.param("id").and_then(|v| v.parse().ok()).unwrap_or(1);

        let row = db
            .select_one("select id, name, email from bench_users where id = $1", &[Value::Int(id)])
            .await?
            .ok_or_else(|| Error::msg("no such user"))?;

        Ok::<_, Error>(Response::json(row.to_json()))
    });

    r.get("/db/posts", |req: Request| async move {
        let db = req.state::<Database>().expect("the database");

        // Two queries, never twenty-one: the posts, then every author they
        // refer to in one go. This is the whole point of the endpoint.
        let posts = db
            .select("select id, title, user_id from bench_posts order by id limit 20", &[])
            .await?;

        let ids: Vec<Value> =
            posts.iter().filter_map(|p| p.get::<i64>("user_id").ok()).map(Value::Int).collect();
        let placeholders: Vec<String> =
            (1..=ids.len()).map(|position| format!("${position}")).collect();

        let authors = db
            .select(
                &format!(
                    "select id, name from bench_users where id in ({})",
                    placeholders.join(", ")
                ),
                &ids,
            )
            .await?;

        let json: Vec<Json> = posts
            .iter()
            .map(|post| {
                let user_id = post.get::<i64>("user_id").unwrap_or_default();
                let author = authors
                    .iter()
                    .find(|a| a.get::<i64>("id").unwrap_or_default() == user_id)
                    .map(|a| {
                        Json::object([
                            ("id", Json::from(user_id as f64)),
                            ("name", Json::from(a.get::<String>("name").unwrap_or_default())),
                        ])
                    })
                    .unwrap_or(Json::Null);

                Json::object([
                    ("id", Json::from(post.get::<i64>("id").unwrap_or_default() as f64)),
                    ("title", Json::from(post.get::<String>("title").unwrap_or_default())),
                    ("author", author),
                ])
            })
            .collect();

        Ok::<_, Error>(Response::json(Json::Array(json)))
    });

    r.get("/template", |req: Request| async move {
        let views = req.state::<Arc<Engine>>().expect("the view engine");

        let rows: Vec<Json> = (1..=50)
            .map(|id| {
                Json::object([
                    ("id", Json::from(id as f64)),
                    ("name", Json::from(format!("User {id}"))),
                ])
            })
            .collect();

        let context = ViewContext::new()
            .with("title", Json::from("Benchmark"))
            .with("rows", Json::Array(rows));

        Ok::<_, Error>(Response::html(views.render("table", &context)?))
    });
}
