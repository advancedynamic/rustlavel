# rustlavel-otel

OpenTelemetry for Rustlavel: traces and metrics exported over OTLP.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.8", features = ["otel"] }
```

## Logs

Traces and metrics are on by default; logs are opt-in, because they can carry anything
an application prints. Enable them with `OpenTelemetry::new().logs(true)`,
`otel.logs.enabled`, or `OTEL_LOGS_EXPORTER=otlp`. A line written while a request is
being handled carries that request's trace and span id, so a collector can show it
beside the trace. Verified against the OpenTelemetry Collector 0.160, in both protobuf
and JSON.

## Documentation

- [API documentation](https://docs.rs/rustlavel-otel)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
