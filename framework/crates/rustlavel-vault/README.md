# rustlavel-vault

Secrets from OpenBao or HashiCorp Vault: KV, dynamic database credentials, leases.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.7", features = ["vault"] }
```

## Documentation

- [API documentation](https://docs.rs/rustlavel-vault)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
