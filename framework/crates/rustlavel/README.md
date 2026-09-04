# rustlavel

A Laravel-inspired full-stack web framework for Rust, written from scratch.

No Axum, no hyper, no SeaORM. Tokio is the only large dependency, and the HTTP/1.1
server, router, template engine, query builder, migrator, ORM and SMTP client are all
written here. Cryptography is the deliberate exception — argon2, sha2, hmac, aes-gcm
and rustls are used rather than reinvented, because writing your own cipher is a
vulnerability, not an achievement.

## A first application

```sh
cargo install rustlavel-cli
rustlavel new blog --with db,view
cd blog && rustlavel serve
```

Or the whole thing — sign-in, passkeys, roles and permissions, an audit trail, a
settings screen, menus and notifications, in about a hundred files you own:

```sh
rustlavel new app --with auth-kit
```

## What a handler looks like

```rust
use rustlavel::prelude::*;

#[rustlavel::main]
async fn main() -> Result<()> {
    App::new()?
        .routes(|r| {
            r.get("/", |_req: Request| async { "Hello" });
        })
        .run()
        .await
}
```

## Packages

Everything optional is a feature flag, and what you do not enable is never compiled:

`ai`, `audit`, `auth`, `cache`, `client`, `db`, `debugbar`, `flags`, `i18n`, `ldap`,
`mail`, `mcp`, `metrics`, `model-cache`, `oauth`, `oauth-provider`, `openapi`, `otel`,
`queue`, `rbac`, `search`, `storage`, `telescope`, `validation`, `vault`, `view`,
`webauthn`, `ws`.

```toml
[dependencies]
rustlavel = { version = "0.7", features = ["db", "view", "auth"] }
```

## Databases

PostgreSQL, MySQL and SQL Server, each with its own wire protocol written here and TLS
through rustls. Oracle is deliberately excluded: its protocol has never been published,
so a driver means wrapping a proprietary library or a multi-year reverse-engineering
effort.

## Documentation

- [API documentation](https://docs.rs/rustlavel)
- [The repository](https://github.com/advancedynamic/rustlavel), including `ROADMAP.md`
  — the design rules, what is built, and what is deliberately left out

## Licence

MIT.
