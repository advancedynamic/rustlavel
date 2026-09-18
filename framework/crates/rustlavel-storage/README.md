# rustlavel-storage

Rustlavel file storage: local disk and S3-compatible object stores.

Part of [Rustlavel](https://github.com/advancedynamic/rustlavel), a full-stack web
framework for Rust written from scratch — no Axum, no hyper, no SeaORM. Tokio is the
only large dependency.

## Large files: let the browser talk to the bucket

A video that passes through the application server costs that server the whole
file in memory and the transfer twice. The right shape is a URL the browser
uploads to directly — the server issues it, the browser uses it, and the server
is never in the path:

```rust
// One request, up to a few hundred MB.
let url = storage.presigned_put("videos/raw/abc.mp4", Duration::from_secs(900))?;
// Hand `url` to the browser; it PUTs the file there.
```

For anything larger, multipart: a dropped connection then costs one part rather
than the whole file, and parts upload in parallel.

```rust
let upload = storage.create_multipart("videos/raw/abc.mp4").await?;
let part_url = storage.presigned_part(&upload, 1, Duration::from_secs(900))?;
// … the browser PUTs each part and keeps each response's ETag header …
storage.complete_multipart(&upload, parts).await?;   // or abort_multipart
```

Every part but the last must be at least 5 MB; there may be at most 10,000.
`presigned_get` does the same for downloads of private objects.

Abandoned uploads are billed until something cleans them up, and raw uploads
usually have a natural lifetime. One rule covers both:

```rust
storage.expire_after("videos/raw/", 60).await?;   // days
```

The signing is AWS Signature V4 in query-string form, tested against the worked
example in AWS's own documentation.

## Using it

Enable it through the meta-crate rather than depending on this one directly, so the
versions stay in step:

```toml
[dependencies]
rustlavel = { version = "0.8", features = ["storage"] }
```

## Documentation

- [API documentation](https://docs.rs/rustlavel-storage)
- [The framework](https://github.com/advancedynamic/rustlavel), including the roadmap
  and the design rules this crate is written under

## Licence

MIT.
