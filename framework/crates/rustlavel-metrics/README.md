# rustlavel-metrics

Prometheus metrics for Rustlavel, gathered from the instrumentation bus.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.7", features = ["metrics"] }
```

## Documentation

- [API documentation](https://docs.rs/rustlavel-metrics)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
