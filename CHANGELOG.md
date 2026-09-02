# Changelog

Notable changes, newest first. Versions follow crates.io; every crate in the
workspace shares one number.

## 0.1.1 — 2026-09-02

The API release: everything an application that only serves JSON needed and
did not have, plus five packages that existed in the repository but had never
been published.

### Added

- **CORS** (`Cors`), configured with the keys of Laravel's `config/cors.php`;
  every list accepts a comma-separated string so it can come from `.env`.
- **Response compression** (`Compress`): gzip and deflate, with a DEFLATE
  codec written in the framework — fixed and dynamic Huffman blocks and a
  complete inflater, checked against zlib's own output.
- **ETag and conditional requests** (`ETag`): `If-None-Match` and
  `If-Modified-Since` answered with `304`. Static files now carry
  `Last-Modified`.
- **API resources** (`JsonResource`, `attributes()`): one JSON shape per type,
  with Laravel's `data`/`meta`/`links` for pagination.
- **Request identifiers** (`RequestId`), on the request, the response, a
  task-local, and the `http.request` instrumentation event.
- **Idempotency keys** (`Idempotency`, in `rustlavel-cache`): a write with an
  `Idempotency-Key` runs once and is replayed thereafter.
- **Health probes** (`Health`): `/up` for liveness, `/up/ready` for readiness
  with concurrent, time-limited checks.
- **`Timeout`** and **`BodyLimit`** middleware, per group.
- **API versioning**: `Router::version`, `VersionHeader`, and
  `Deprecation`/`Sunset` headers from `.deprecated_at()` and `.sunset()`.
- `#[rustlavel::main]` and `#[rustlavel::test]`, with `tokio` re-exported, so
  an application's dependency list is one line.
- `Config::list`, for values that are an array in JSON and a comma-separated
  string in `.env`.
- `SearchClient::from_config` and `RelyingParty::from_config`.
- `rustlavel-client` asks for and transparently decodes gzip and deflate.
- First publication of `rustlavel-debugbar`, `rustlavel-ldap`,
  `rustlavel-otel`, `rustlavel-search` and `rustlavel-webauthn`.

### Fixed

- A warning from the framework's own code no longer appears in an
  application's build when the `db` and `queue` packages are both off.

## 0.1.0 — 2026-08-30

First release.
