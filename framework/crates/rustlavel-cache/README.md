# rustlavel-cache

Rustlavel cache: memory, file and a from-scratch Redis driver — which is also the Valkey driver — plus rate limiting.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Valkey

There is no separate Valkey package, and that is a decision rather than a gap.
Valkey is the Linux Foundation fork of Redis 7.2 and speaks RESP unchanged, so a
second client would be this one with the name changed — and two copies of a
protocol implementation drift apart exactly once, at the worst moment. Instead
the one client answers to both names:

```sh
CACHE_DRIVER=valkey
CACHE_URL=valkey://:secret@cache.internal:6379/0
```

`valkey://` and `redis://` parse identically; `VALKEY_URL` is read where
`REDIS_URL` is; `features = ["valkey"]` on the meta-crate enables this package.
The whole integration suite — the cache contract, the rate limiter, sixteen
tasks sharing one pool, the guard against a key smuggling a second command — is
run against Valkey 8 and passes unchanged.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.8", features = ["cache"] }
```

## Documentation

- [API documentation](https://docs.rs/rustlavel-cache)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
