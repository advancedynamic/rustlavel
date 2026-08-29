# Rustlavel

A Laravel-inspired web framework for Rust — built from scratch.

> As comfortable as Laravel while you write it. As calm as Rust when you deploy it.

Rustlavel takes what makes Laravel a pleasure to work in — conventions, an
Artisan-style CLI, migrations, an Eloquent-shaped ORM, Blade-shaped templates,
a debugging dashboard — and rebuilds it on Rust's terms. Not a translation:
where Laravel resolves things at runtime through reflection, Rustlavel resolves
them at compile time, so a renamed column or a typo'd service is a build error
rather than a `null` in production.

**From scratch means from scratch.** The HTTP server, the router, the JSON
parser, the `.env` loader, the PostgreSQL and MySQL and TDS wire protocols, the
template engine, the derive macros — all written here. The only dependencies are
Tokio for the async runtime and a small set of cryptography crates, because
hand-rolling a cipher or a KDF would be a vulnerability, not an achievement.

## A first look

```bash
cargo install rustlavel-cli
rustlavel new blog
cd blog
rustlavel serve          # http://localhost:8000, reloads when you save
```

`src/routes/web.rs`:

```rust
use rustlavel::prelude::*;

pub fn routes(r: &mut Router) {
    r.get("/", home).name("home");
    r.get("/posts/{id}", PostController::show);

    r.group("/admin", |r| {
        r.middleware(auth);
        r.get("/dashboard", DashboardController::index);
    });
}
```

`src/main.rs`:

```rust
use rustlavel::prelude::*;
use blog::routes;

#[tokio::main]
async fn main() -> Result<()> {
    App::new()?.routes(routes::web::routes).run().await
}
```

## Packages, not a monolith

Like `composer require`, every feature beyond the core is opt-in — and what you
do not add is never compiled into your binary.

```bash
rustlavel new blog --with db,view,auth      # choose them at scaffold time
cargo add rustlavel-telescope               # or add one later
```

```rust
App::new()?
    .routes(routes::web::routes)
    .plugin(Telescope::default())   // one explicit line, no runtime discovery
    .run()
    .await
```

| Crate | What it gives you |
| --- | --- |
| `rustlavel` | The meta-crate an application imports, with feature flags |
| `rustlavel-core` | Config, `.env`, JSON, typed context, instrumentation bus, events |
| `rustlavel-http` | HTTP/1.1 server, router, middleware, dev error page, test client |
| `rustlavel-cli` | The `rustlavel` binary — `new`, `serve`, `make:*`, `migrate`, `doctor`, `build` |
| `rustlavel-db` | Drivers for PostgreSQL, MySQL and SQL Server; query builder, schema, migrations, ORM, pagination |
| `rustlavel-view` | Blade-shaped template engine |
| `rustlavel-validation` | Laravel-style rules and 422 responses |
| `rustlavel-auth` | Password hashing, encryption, sessions, CSRF, signed URLs, guards, API tokens |
| `rustlavel-cache` | Memory, file, and a from-scratch Redis client; rate limiting |
| `rustlavel-queue` | Background jobs, workers, retries, dead letters, cron scheduling |
| `rustlavel-mail` | SMTP written from scratch, MIME, mailables, notifications |
| `rustlavel-storage` | Local disk and S3-compatible object stores |
| `rustlavel-client` | Outbound HTTP with TLS, streaming, and `Http::fake()` |
| `rustlavel-ai` | Anthropic, OpenAI and Ollama through one API |
| `rustlavel-mcp` | Model Context Protocol, server and client |
| `rustlavel-oauth` | Sign in through Google, GitHub and the rest — OAuth 2.1 with mandatory PKCE |
| `rustlavel-oauth-provider` | Be the provider: authorization code, refresh rotation, revocation, introspection |
| `rustlavel-telescope` | The debugging dashboard |
| `rustlavel-metrics` | Prometheus metrics from the events already being emitted |
| `rustlavel-openapi` | API documentation generated from the routes |
| `rustlavel-ws` | WebSocket and broadcasting — private and presence channels |
| `rustlavel-i18n` | Translations, plurals, locale detection |
| `rustlavel-macros` | `#[derive(Model)]` |

## What it looks like in practice

**Migrations** read the way they do in Laravel:

```rust
migration!(
    CreateUsersTable,
    "2026_08_29_120000_create_users_table",
    up: |schema| {
        schema.create("users", |t| {
            t.id();
            t.string("name");
            t.string("email").unique();
            t.timestamps();
        }).await
    },
    down: |schema| { schema.drop("users").await },
);
```

**Models** are plain structs the compiler understands:

```rust
#[derive(Model, Default)]
#[model(table = "users")]
pub struct User {
    #[model(primary_key, generated)]
    pub id: i64,
    pub name: String,
    pub email: Option<String>,
}

let user = User::find(&db, 7).await?;
let posts = has_many::<User, Post>(&db, &users, "user_id").await?;  // two queries, never N+1
```

**Queries** cannot be injected into, by construction — values are always bound
and identifiers are validated before they are quoted. The same chain produces
correct SQL on PostgreSQL, MySQL and SQL Server; only the URL changes:

```rust
db.table("posts")
    .filter("published", true)
    .filter_op("views", ">", 100)
    .latest("created_at")
    .paginate(&db, page, 20)
    .await?
```

**Tests** cost about as much as calling a function — no socket, no port:

```rust
client.get("/posts/7").await.assert_ok().assert_json("title", "Hello");
```

## What it does differently from Laravel

Some of Laravel's most-loved mechanisms depend on a dynamic language. Rather
than fake them, Rustlavel replaces them with something that fits:

| Laravel | Rustlavel | Why |
| --- | --- | --- |
| Facades, service container | Typed context: `req.state::<Database>()` | Resolved at compile time; a missing service will not compile |
| Package auto-discovery | One explicit `.plugin(...)` line | No runtime reflection exists; explicit is also easier to follow |
| Directory-scanned migrations | A registry the CLI generates | A compiled program cannot list a directory into types — but you never edit the file |
| `DB::transaction(closure)` | A transaction guard (`begin`/`commit`) | A closure returning a future that borrows its argument produces an unreadable error |
| `php artisan tinker` | Not available | Rust has no REPL |

And some things fall out of Rust that Laravel cannot offer: a single binary to
deploy with no runtime to install, and a compiler that catches a renamed column
before the request does.

## Databases

```bash
DATABASE_URL=postgres://user:password@host/db
DATABASE_URL=mysql://user:password@host/db
DATABASE_URL=sqlserver://user:password@host/db
```

The scheme picks the driver and its default port. Above the driver line the
framework is one codebase — the query builder, schema builder, migrator and ORM
are written once — and a `Dialect` supplies the handful of things the databases
genuinely disagree about.

**Oracle is deliberately not supported.** Its network protocol has never been
published, so a driver means either wrapping the proprietary OCI library or a
multi-year reverse-engineering effort. Neither fits a framework whose premise is
that the protocols are written here. [ROADMAP.md](ROADMAP.md) records the
reasoning in full.

## Status

Early, but broad. Everything in the table above works today and is covered by
tests — over 1,400 of them, including integration suites against real
PostgreSQL, MySQL, SQL Server and Redis servers. See [ROADMAP.md](ROADMAP.md)
for what has landed and what has not.

The database layer additionally carries a conformance suite: the same schema
and query code runs against every configured server, so "the generated SQL
looks right" and "the generated SQL works" are checked separately. Writing it
found six bugs the unit tests were perfectly happy with — among them a `t.foreign_id(...)`
that produced no foreign key at all on MySQL, and a migrator whose transaction
spanned four different pooled connections.

Not there yet: passkeys and an auth starter kit, re-rendering an HTML form with
its validation errors, a Livewire-style component layer, and a documentation
site.

```bash
cd framework
cargo test --workspace --all-features
```

## Inspiration

Laravel 13 for the shape and the conventions, Loco.rs for showing this is worth
doing in Rust, and Ignition for proving that a good error page is a feature.

## License

MIT
