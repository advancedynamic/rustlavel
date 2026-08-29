# Blog — a Rustlavel example

A small but complete application: a model, a migration, a controller, form
validation, Blade-shaped templates, and tests. It exists to prove the framework
works end to end, and to be the thing to read when the documentation does not
answer a question.

## Running it

```bash
export DATABASE_URL=postgres://user:password@localhost/blog
cargo run -- migrate
cargo run                           # http://localhost:8000
```

## What to look at

| File | What it shows |
| --- | --- |
| `src/routes/web.rs` | Routes, named |
| `src/models/post.rs` | `#[derive(Model)]` and a query scope |
| `src/controllers/post_controller.rs` | Handlers, validation, redirects |
| `database/migrations/` | A schema written in Rust |
| `resources/views/` | Layout inheritance and automatic escaping |
| `tests/blog.rs` | HTTP tests that never open a socket |
