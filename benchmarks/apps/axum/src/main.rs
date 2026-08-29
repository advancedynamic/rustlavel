//! Axum's entry in the benchmark. See `benchmarks/CONTRACT.md`.
//!
//! Written the way an Axum user would write it — the ordinary crates
//! (axum, tokio, serde, sqlx, askama) used in the ordinary way — rather than
//! tuned for the measurement.

use std::collections::HashMap;
use std::net::SocketAddr;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a number");
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");

    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(&url)
        .await
        .expect("connecting to PostgreSQL");

    // Five middlewares, as the contract requires. Layered onto a router that
    // holds only `/middleware`, so the other endpoints are not measured
    // through them.
    let middleware_routes = Router::new()
        .route("/middleware", get(middleware_handler))
        .layer(middleware::from_fn(bench_header::<5>))
        .layer(middleware::from_fn(bench_header::<4>))
        .layer(middleware::from_fn(bench_header::<3>))
        .layer(middleware::from_fn(bench_header::<2>))
        .layer(middleware::from_fn(bench_header::<1>));

    let app = Router::new()
        .route("/plaintext", get(plaintext))
        .route("/json", get(json))
        .route("/users/{id}/posts/{slug}", get(user_post))
        .route("/json-big", get(json_big))
        .route("/db/user/{id}", get(db_user))
        .route("/db/posts", get(db_posts))
        .route("/template", get(template))
        .merge(middleware_routes)
        .with_state(pool);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await.expect("binding the port");
    axum::serve(listener, app).await.expect("serving");
}

// 1. GET /plaintext

async fn plaintext() -> &'static str {
    "Hello, World!"
}

// 2. GET /json

#[derive(Serialize)]
struct Message {
    message: &'static str,
}

async fn json() -> Json<Message> {
    Json(Message { message: "Hello, World!" })
}

// 3. GET /users/{id}/posts/{slug}

#[derive(Serialize)]
struct UserPost {
    id: i64,
    slug: String,
}

async fn user_post(Path((id, slug)): Path<(i64, String)>) -> Json<UserPost> {
    Json(UserPost { id, slug })
}

// 4. GET /middleware

#[derive(Serialize)]
struct Depth {
    depth: u32,
}

async fn middleware_handler() -> Json<Depth> {
    Json(Depth { depth: 5 })
}

/// One real middleware layer: run the rest of the stack, then set one header.
async fn bench_header<const N: u8>(
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    let name = match N {
        1 => HeaderName::from_static("x-bench-1"),
        2 => HeaderName::from_static("x-bench-2"),
        3 => HeaderName::from_static("x-bench-3"),
        4 => HeaderName::from_static("x-bench-4"),
        _ => HeaderName::from_static("x-bench-5"),
    };
    response.headers_mut().insert(name, HeaderValue::from_static("ok"));
    response
}

// 5. GET /json-big

#[derive(Serialize)]
struct BigUser {
    id: i64,
    name: String,
    email: String,
    active: bool,
    score: f64,
}

async fn json_big() -> Json<Vec<BigUser>> {
    let rows = (1..=100i64)
        .map(|id| BigUser {
            id,
            name: format!("User {id}"),
            email: format!("user{id}@example.test"),
            active: id % 2 == 0,
            score: id as f64 * 1.5,
        })
        .collect();
    Json(rows)
}

// 6. GET /db/user/{id}

#[derive(Serialize)]
struct DbUser {
    id: i32,
    name: String,
    email: String,
}

async fn db_user(
    State(pool): State<PgPool>,
    Path(id): Path<i32>,
) -> Result<Json<DbUser>, AppError> {
    let row = sqlx::query("select id, name, email from bench_users where id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await?;

    Ok(Json(DbUser { id: row.get("id"), name: row.get("name"), email: row.get("email") }))
}

// 7. GET /db/posts

#[derive(Serialize)]
struct Author {
    id: i32,
    name: String,
}

#[derive(Serialize)]
struct PostWithAuthor {
    id: i32,
    title: String,
    author: Option<Author>,
}

async fn db_posts(State(pool): State<PgPool>) -> Result<Json<Vec<PostWithAuthor>>, AppError> {
    // Two queries, never twenty-one: the posts, then every author they refer
    // to in one go. This is the whole point of the endpoint.
    let posts = sqlx::query("select id, title, user_id from bench_posts order by id limit 20")
        .fetch_all(&pool)
        .await?;

    let ids: Vec<i32> = posts.iter().map(|row| row.get::<i32, _>("user_id")).collect();

    let authors = sqlx::query("select id, name from bench_users where id = any($1)")
        .bind(&ids)
        .fetch_all(&pool)
        .await?;

    let names: HashMap<i32, String> =
        authors.iter().map(|row| (row.get("id"), row.get("name"))).collect();

    let out = posts
        .iter()
        .map(|row| {
            let user_id: i32 = row.get("user_id");
            PostWithAuthor {
                id: row.get("id"),
                title: row.get("title"),
                author: names
                    .get(&user_id)
                    .map(|name| Author { id: user_id, name: name.clone() }),
            }
        })
        .collect();

    Ok(Json(out))
}

// 8. GET /template

struct TemplateRow {
    id: i64,
    name: String,
}

#[derive(Template)]
#[template(path = "table.html")]
struct TableTemplate {
    title: &'static str,
    rows: Vec<TemplateRow>,
}

async fn template() -> Result<Html<String>, AppError> {
    let rows = (1..=50i64)
        .map(|id| TemplateRow { id, name: format!("User {id}") })
        .collect();

    let page = TableTemplate { title: "Benchmark", rows }.render()?;
    Ok(Html(page))
}

// Errors

struct AppError(String);

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        AppError(error.to_string())
    }
}

impl From<askama::Error> for AppError {
    fn from(error: askama::Error) -> Self {
        AppError(error.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0).into_response()
    }
}
