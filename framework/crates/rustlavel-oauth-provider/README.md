# rustlavel-oauth-provider

Be an OAuth 2.1 provider: authorization code + PKCE, refresh rotation, revocation, introspection.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.7", features = ["oauth-provider"] }
```

## Where the tokens live

The stores are a trait each, and there are two implementations of every one.

The `Memory*` stores are the default and are right for a test and for a
development server. They are wrong for anything that restarts: an empty store
answers "unknown token" to everything, so a deploy would sign out every session
and break every integration at once.

The `Database*` stores, behind the `db` feature, keep clients, codes, access and
refresh tokens and consent in a table each. `database::schema` creates them, so
a migration and the code that reads it cannot drift apart.

```rust
AuthorizationServer::new(DatabaseClientStore::new(db.clone()))
    .storing_codes(DatabaseCodeStore::new(db.clone()))
    .storing_tokens(DatabaseTokenStore::new(db.clone()))
    .storing_consent(DatabaseConsentStore::new(db))
```

**Two operations are atomic, and have to be.** Spending an authorization code
and rotating a refresh token are each one `UPDATE … WHERE <still unspent>` and a
check of how many rows changed, so the database picks the winner — the only
party that can. A read followed by a write passes every single-threaded test and
leaves the window that is the whole attack: with sixteen requests presenting one
code at once, that shape lets eight of them through.

If you write a store of your own, that is the property to preserve.

## Documentation

- [API documentation](https://docs.rs/rustlavel-oauth-provider)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
