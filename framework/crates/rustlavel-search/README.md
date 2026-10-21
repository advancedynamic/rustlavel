# rustlavel-search

Elasticsearch and OpenSearch for Rustlavel: indexing, search, and bulk operations.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.9", features = ["search"] }
```

## Shipping logs

Every `info!`, `warn!` and `error!` is an event on the framework's bus, so shipping
logs to the cluster is a subscriber. It writes one document per line to a daily index
(`logs-2026.10.21`), shaped after the Elastic Common Schema's common fields, in
batches, with a bounded queue that drops the newest line and counts it rather than
slowing a request.

```toml
# config/search.json
{ "url": "http://localhost:9200", "logs": { "enabled": true, "index": "logs" } }
```

```rust
App::new()?.plugin(rustlavel::search::LogShipping::default()).run().await
```

Off unless `search.logs.enabled` is true. Flushed when the server shuts down.
Verified against Elasticsearch 8.15 and OpenSearch 2.17.

## Documentation

- [API documentation](https://docs.rs/rustlavel-search)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
