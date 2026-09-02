# Changelog

Notable changes, newest first. Versions follow crates.io; every crate in the
workspace shares one number.

## 0.2.0 — 2026-09-02

The minor number rather than the patch, and deliberately: for a `0.x`
version Cargo treats the minor as the major, so `rustlavel = "0.1"` would
have picked this up on its own. Two changes below need a person to read them
first.

### Breaking

- **`Request::ip()` no longer reads `X-Forwarded-For` by itself.** An
  application behind a load balancer must add the new `TrustProxies`
  middleware, or it will see the balancer's address for every request. See
  below for why this is worth the interruption.
- **`Error` has a new variant, `Unavailable`.** Code that matches on `Error`
  exhaustively will not compile until it handles it.

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
- **A circuit breaker** for outbound calls, in `rustlavel-client`:
  `Client::new().breaker(CircuitBreaker::new())`. It trips on a failure rate
  over a sliding window rather than a raw count, keeps one breaker per host,
  and probes with a few calls before resuming. A 5xx counts against the
  upstream; a 4xx does not.
- `Error::Unavailable`, raised when a call was refused rather than attempted,
  so a caller can safely fall back. It renders as a 503.

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
