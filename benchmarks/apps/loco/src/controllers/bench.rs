//! Loco's entry in the benchmark. See `benchmarks/CONTRACT.md`.
//!
//! Written the way a Loco application is written — a controller returning
//! `Routes`, `format::` responses, SeaORM entities and Loco's own Tera view
//! layer — and not tuned for the measurement.

use axum::{extract::Request, middleware::Next};
use loco_rs::prelude::*;
use sea_orm::{QueryOrder, QuerySelect};

use crate::models::_entities::{bench_posts, bench_users};
use crate::views::bench::{Author, BigRow, Depth, Message, Params, Post, TemplateRow};

#[debug_handler]
async fn plaintext() -> Result<Response> {
    format::text("Hello, World!")
}

#[debug_handler]
async fn json() -> Result<Response> {
    format::json(Message {
        message: "Hello, World!",
    })
}

#[debug_handler]
async fn params(Path((id, slug)): Path<(i64, String)>) -> Result<Response> {
    format::json(Params { id, slug })
}

#[debug_handler]
async fn middleware() -> Result<Response> {
    format::json(Depth { depth: 5 })
}

#[debug_handler]
async fn json_big() -> Result<Response> {
    let rows: Vec<BigRow> = (1..=100).map(BigRow::new).collect();
    format::json(rows)
}

#[debug_handler]
async fn db_user(State(ctx): State<AppContext>, Path(id): Path<i32>) -> Result<Response> {
    let user = bench_users::Entity::find_by_id(id)
        .one(&ctx.db)
        .await?
        .ok_or_else(|| Error::NotFound)?;

    format::json(user)
}

#[debug_handler]
async fn db_posts(State(ctx): State<AppContext>) -> Result<Response> {
    // `find_also_related` is SeaORM's eager load: one LEFT JOIN, one query, no
    // N+1. The contract allows two; this needs one.
    let rows = bench_posts::Entity::find()
        .order_by_asc(bench_posts::Column::Id)
        .limit(20)
        .find_also_related(bench_users::Entity)
        .all(&ctx.db)
        .await?;

    let posts: Vec<Post> = rows
        .into_iter()
        .map(|(post, author)| Post {
            id: post.id,
            title: post.title,
            author: author.map(|author| Author {
                id: author.id,
                name: author.name,
            }),
        })
        .collect();

    format::json(posts)
}

#[debug_handler]
async fn template(ViewEngine(v): ViewEngine<TeraView>) -> Result<Response> {
    let rows: Vec<TemplateRow> = (1..=50)
        .map(|id| TemplateRow {
            id,
            name: format!("User {id}"),
        })
        .collect();

    format::render().view(&v, "bench/table.html", data!({"title": "Benchmark", "rows": rows}))
}

/// One of the five layers `/middleware` sits behind. Each sets a single
/// response header and passes the request on.
macro_rules! bench_header {
    ($name:ident, $header:literal) => {
        async fn $name(req: Request, next: Next) -> axum::response::Response {
            let mut response = next.run(req).await;
            response.headers_mut().insert(
                axum::http::HeaderName::from_static($header),
                axum::http::HeaderValue::from_static("ok"),
            );
            response
        }
    };
}

bench_header!(bench_1, "x-bench-1");
bench_header!(bench_2, "x-bench-2");
bench_header!(bench_3, "x-bench-3");
bench_header!(bench_4, "x-bench-4");
bench_header!(bench_5, "x-bench-5");

pub fn routes() -> Routes {
    Routes::new()
        .add("/plaintext", get(plaintext))
        .add("/json", get(json))
        .add("/users/{id}/posts/{slug}", get(params))
        .add("/json-big", get(json_big))
        .add("/db/user/{id}", get(db_user))
        .add("/db/posts", get(db_posts))
        .add("/template", get(template))
}

/// `/middleware` is its own `Routes` so the five layers wrap only it — the
/// other seven endpoints are not measured through a stack the contract does
/// not ask them to carry.
pub fn middleware_routes() -> Routes {
    Routes::new()
        .add("/middleware", get(middleware))
        .layer(axum::middleware::from_fn(bench_1))
        .layer(axum::middleware::from_fn(bench_2))
        .layer(axum::middleware::from_fn(bench_3))
        .layer(axum::middleware::from_fn(bench_4))
        .layer(axum::middleware::from_fn(bench_5))
}
