# rustlavel-ai

Rustlavel AI: one API for Anthropic, OpenAI and Ollama, with streaming, tools and a fake provider.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.7", features = ["ai"] }
```

## Documentation

- [API documentation](https://docs.rs/rustlavel-ai)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
