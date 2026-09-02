# Rustlavel

[![Support via PayPal](https://img.shields.io/badge/PayPal-Support-00457C?style=for-the-badge&logo=paypal&logoColor=white)](https://paypal.me/abahido)
[![Support via Lynk.id](https://img.shields.io/badge/Lynk.id-Support-FB6B35?style=for-the-badge&logo=kofi&logoColor=white)](https://lynk.id/abahido/s/z52m3ekew032)

A Laravel-inspired web framework for Rust — built from scratch.

**[Documentation](https://advancedynamic.github.io/rustlavel/)** ·
[Guide](https://advancedynamic.github.io/rustlavel/guide.html) ·
[Packages](https://advancedynamic.github.io/rustlavel/packages.html) ·
[API reference](https://docs.rs/rustlavel)

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

## A starter kit, if you want one

```bash
rustlavel new app --with auth-kit
```

Sign-in with an authenticator app or a passkey, recovery codes, invitation and
self-registration, password reset, a sign-in audit log, account lockout, roles
and permissions with a management area, "view as this user", and eleven pages of
Tailwind. The controllers and views are written into your project — the login
page is a file you edit, not a template you override.

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
| `rustlavel-http` | HTTP/1.1 server, router, middleware, dev error page, test client; CORS, gzip (a DEFLATE written here), ETag, request ids, health probes, timeouts, API resources and versioning |
| `rustlavel-cli` | The `rustlavel` binary — `new`, `serve`, `make:*`, `migrate`, `doctor`, `build` |
| `rustlavel-db` | Drivers for PostgreSQL, MySQL and SQL Server with TLS; query builder, schema, migrations, ORM, pagination |
| `rustlavel-view` | Blade-shaped template engine |
| `rustlavel-validation` | Laravel-style rules and 422 responses |
| `rustlavel-auth` | Password hashing, encryption, sessions, CSRF, signed URLs, guards, API tokens |
| `rustlavel-cache` | Memory, file, and a from-scratch Redis client; rate limiting and idempotency keys |
| `rustlavel-rbac` | Roles and permissions: assignment, wildcard checks, and route guards |
| `rustlavel-queue` | Background jobs, workers, retries, dead letters, cron scheduling |
| `rustlavel-mail` | SMTP written from scratch, MIME, mailables, notifications |
| `rustlavel-storage` | Local disk and S3-compatible object stores |
| `rustlavel-client` | Outbound HTTP with TLS, streaming, and `Http::fake()`; circuit breaker |
| `rustlavel-ai` | Anthropic, OpenAI and Ollama through one API |
| `rustlavel-mcp` | Model Context Protocol, server and client |
| `rustlavel-oauth` | Sign in through Google, GitHub and the rest — OAuth 2.1 with mandatory PKCE |
| `rustlavel-oauth-provider` | Be the provider: authorization code, refresh rotation, revocation, introspection |
| `rustlavel-vault` | Secrets from OpenBao or HashiCorp Vault, including dynamic database credentials |
| `rustlavel-webauthn` | Passkeys — WebAuthn registration and authentication |
| `rustlavel-ldap` | LDAP v3 with BER written here; authenticating against a directory |
| `rustlavel-search` | Elasticsearch and OpenSearch |
| `rustlavel-otel` | Traces and metrics over OTLP, from the events already being emitted |
| `rustlavel-debugbar` | A development overlay on the page: this request's queries, cache, timing |
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

## Benchmarks

Eight endpoints, six deployments, on one developer machine. Every app implements
the same [contract](benchmarks/CONTRACT.md) — identical responses, a pool of 16,
release builds throughout — and the harness refuses to measure an app until all
eight endpoints answer 200, because a 500 error page benchmarks as very fast.

Requests per second, higher is better. Two independent runs; the second is shown
and the first agreed to within 1% on every database row.

| | Rustlavel | Axum | Loco | Spring Boot | Laravel FPM | Laravel Octane |
|---|---|---|---|---|---|---|
| Plaintext | 123,484 | 123,541 | 120,700 | 94,807 | 5,588 | 11,905 |
| JSON | 123,342 | 122,940 | 120,801 | 94,793 | 4,818 | 12,237 |
| Routing | 123,421 | 122,519 | 121,050 | 95,649 | 5,845 | 11,683 |
| Middleware ×5 | 123,793 | 121,513 | 124,877 | 95,508 | 5,480 | 10,446 |
| **JSON ×100** | **85,337** | 124,950 | 113,060 | 80,538 | 4,925 | 9,894 |
| **DB, one row** | **26,458** | 10,501 | 10,212 | 24,867 | 656 | 4,706 |
| **DB, relations** | **11,953** | 5,245 | 9,668 | 11,899 | 819 | 2,146 |
| Template | 95,871 | 119,682 | 116,003 | 37,604 | 5,343 | 8,210 |

| | Rustlavel | Axum | Loco | Spring Boot | Laravel FPM | Laravel Octane |
|---|---|---|---|---|---|---|
| Startup | 121 ms | 113 ms | 109 ms | 1,829 ms | 587 ms | 1,027 ms |
| Memory | 17 MB | 18 MB | 30 MB | 632 MB | — | 60 MB |
| Artifact | 4 MB | 2 MB | 14 MB | 26 MB | 44 MB | 44 MB |

### Where this framework loses

**JSON serialisation, by about 1.5×.** `serde_json` is faster than the parser
written here, and Loco uses serde too, so that column is a clean like-for-like.
This is the measurable price of writing it from scratch, and it was predicted
before anything was measured rather than explained afterwards.

**Templates, by about 1.2×** against Askama — which compiles templates into Rust
at build time, so it is not really the same kind of thing. Against Thymeleaf,
the runtime engine in the comparison, it is 2.5× ahead.

### Where it wins, and why that was checked

**Database queries, by 2.5× over sqlx.** A result that flatters the framework
its own author wrote deserves more suspicion than the rest, so it was checked
rather than published: with `log_statement='all'`, both apps issue **exactly one
statement per request**. sqlx uses a cached named prepared statement
(`sqlx_s_1`); this driver uses the unnamed one. So it does *more* protocol work,
not less, and still wins. The result stands.

Spring Boot's `JdbcClient` lands in the same place, which is the useful
corroboration: two very different stacks agree, and the two sqlx-based ones agree
with each other.

### The part that needs no benchmark

`loco-rs` depends unconditionally on `lettre` (SMTP), `opendal` (S3, Azure, GCS),
`argon2` and `notify` — none behind a feature flag. A Loco application with no
mailer and no object storage ships both anyway: 14 MB against 4 MB here. That is
the whole "opt-in packages" premise, visible without timing anything.

### Reading these honestly

- The top four rows are a **ceiling**, not a result. All three Rust frameworks
  land within 3% of each other around 123,000, which is the loopback interface
  and the load generator sharing a CPU with the server. Read a tie there as "no
  measurable difference on this hardware".
- Laravel FPM's memory is missing because the measurement was wrong, not small:
  PHP-FPM workers are not children of the process the harness watches.
- Laravel FPM exhausted the machine's ephemeral ports under sustained load
  (`os error 49`) on two of three database runs — a real property of opening a
  connection per request, but its 656 req/s is not a clean number.
- One developer machine, PostgreSQL in a VM, load generator on the same box. The
  ratios travel; the absolute numbers do not.
- Written by the author of one of the frameworks compared. The method, the apps
  and the raw output are all in [`benchmarks/`](benchmarks/) so the numbers can
  be disagreed with.

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

## Support

Rustlavel is written and maintained in the open. If it is useful to you, or you
would like it to keep going, you can help fund the work:

[![Support via PayPal](https://img.shields.io/badge/PayPal-Support-00457C?style=for-the-badge&logo=paypal&logoColor=white)](https://paypal.me/abahido)
[![Support via Lynk.id](https://img.shields.io/badge/Lynk.id-Support-FB6B35?style=for-the-badge&logo=kofi&logoColor=white)](https://lynk.id/abahido/s/z52m3ekew032)

## License

MIT — see [LICENSE](LICENSE).
