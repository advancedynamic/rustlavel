# Changelog

Notable changes, newest first. Versions follow crates.io; every crate in the
workspace shares one number.

## Unreleased

### Security

- **`Request::ip()` no longer trusts `X-Forwarded-For` on its own.** It did,
  unconditionally, which meant any client could choose its own address by
  sending a header — and the rate limiter and idempotency scoping both key on
  it, so both were escapable by varying one header per request. The address is
  now the peer that opened the socket unless the new `TrustProxies` middleware
  establishes that the connection came from a proxy named in advance.
  Applications behind a load balancer must add `TrustProxies::from_config` (or
  `::at([...])`) to keep seeing real client addresses.

### Added

- `TrustProxies`, with CIDR matching for IPv4 and IPv6, reading
  `trustedproxy.proxies`. Forwarded hops are stripped from the right, so a
  value the client wrote before the first proxy appended to it is never taken
  as the client address.
- `Request::scheme()`, `is_secure()`, `forwarded_host()`, `forwarded_port()`,
  from a trusted proxy's `X-Forwarded-Proto`/`-Host`/`-Port`.
- `Request::with_peer`, so a test can say where a request came from.

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
