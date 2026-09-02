# Changelog

Notable changes, newest first. Versions follow crates.io; every crate in the
workspace shares one number.

## 0.2.2 — 2026-09-02

Two bugs that met anybody who ran `rustlavel new --with auth-kit` on 0.2.1.
Nothing else changed.

### Fixed

- The starter kit shipped a `match` that clippy rewrites as `matches!`, so
  `cargo clippy -- -D warnings` failed on a freshly scaffolded project. A lint
  in the kit is a lint in an application's own build.
- The scaffold's default test asserted a public home page, which the kit
  replaces with a redirect to the sign-in page, so `cargo test` failed
  immediately after `rustlavel new --with auth-kit`. The kit now ships four
  tests of its own covering the guard and the CSRF check.

### Added

- A CI job that scaffolds a project with the kit, builds it, runs its tests and
  runs clippy over it. None of the kit's ~57 files are compiled by the
  workspace, so a rename in the framework used to break them silently.

## 0.2.1 — 2026-09-02

The patch number rather than the minor, and that is the right slot: for a
`0.x` version Cargo treats the minor as the major, so a breaking change would
have to be `0.3.0`. Nothing here breaks anything. Everything below is new, so
`rustlavel = "0.2"` picks it up on its own.

### Added

- **`rustlavel new app --with auth-kit`** — a starter kit, in the shape Laravel
  Breeze has: the controllers, views, routes and migrations are written into
  the project and belong to it, so the login page is a file you edit rather
  than a template you override. The library half stays in crates, so a
  security fix arrives with `cargo update`.
  - Sign-in with an authenticator app (TOTP), passkeys, and single-use recovery
    codes; a sign-in audit log; per-account and per-address lockout.
  - Self-registration (switchable off) and admin invitation, both ending at a
    link where the person chooses their own password — so an administrator
    never knows one.
  - Roles and permissions with a management area, direct per-user grants and
    denies, and "view as this user" for support.
  - Eleven pages of Tailwind v4, built to a committed stylesheet so no Node
    toolchain is needed, under a Content-Security-Policy with no
    `unsafe-inline` anywhere.
- **`rustlavel-rbac`** — roles, permissions, wildcard matching, a cached check
  and `Can::permission(...)` route guards. Tested against PostgreSQL, MySQL and
  SQL Server.
- **`rustlavel-auth::totp`** — RFC 6238, verified against the full RFC 4226 and
  RFC 6238 test tables, with recovery codes.
- **`rustlavel-auth::qr`** — a QR encoder, checked byte for byte against
  libqrencode across 291 payloads. It exists because posting a TOTP secret to a
  chart API is not acceptable and a JavaScript QR library would need a looser
  policy than the kit ships with.
- **`rustlavel-auth::impersonation`** — acting as another user, with the real
  operator kept for the audit trail.
- `CoseKey::to_bytes` and a CBOR encoder, without which a passkey could be
  verified and never stored.
- `RouteHandle::middleware`, for a resource whose verbs need different guards.
- `Request::inputs`, for the repeated fields a checkbox group sends.
- `Config::list`, `Request::body_bytes` in tests, `sha256_hex`.

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
