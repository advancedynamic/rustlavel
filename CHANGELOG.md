# Changelog

Notable changes, newest first. Versions follow crates.io; every crate in the
workspace shares one number.

## 0.5.0 — 2026-09-03

A second-level cache for models, and three things that were switched on and
doing nothing.

### Added

- **`rustlavel-model-cache`** — Hibernate's second-level cache, in this
  framework's vocabulary. Entities kept by primary key and query results kept
  by the SQL that produced them, over the `Cache` trait that already exists, so
  memory, file and Redis all work with no new configuration.

  Caching an entity is the easy half: one key, and the write that changes it
  knows which key to drop. Caching a *query* is the hard one, because a write
  to one row invalidates an unknown number of cached result sets and there is
  no way to enumerate them — a cached `WHERE role = 'admin'` is invalidated by
  an insert whose shape the cache never sees. The answer is Hibernate's: a
  **generation counter per table**, recorded on every cached result and bumped
  by every write. One counter invalidates every stale result set at once.
  Blunt on purpose — a write to any row of `users` costs you every cached
  `users` query, which beats serving a list that is missing a row somebody
  just added.

  The query key is a 64-bit fingerprint *and* the statement, stored beside the
  rows and compared on the way out, so a collision is a miss rather than one
  query's rows served as another's. A model with no registered region is not
  cached at all: a package that quietly starts holding every table the moment
  it is registered is a cache nobody chose.

  Six integration tests run against PostgreSQL 16, MySQL 8.4 and Azure SQL
  Edge, and prove the two things unit tests cannot — that a second read does
  not reach the database, and that a write makes it reach the database again.

### Fixed

- **A model with a `bool` field could not be read on MySQL.** MySQL has no
  boolean: `BOOLEAN` is an alias for `tinyint(1)`, and nothing on the wire
  tells a flag from a small counter, so the driver hands back an integer. The
  model does know — it declared the field `bool` — and that is the layer where
  the two are now reconciled. Zero and one only: a counter column declared
  `bool` is a mistake, and guessing `true` for 7 would hide it.
- **A package the scaffold was asked for was never registered.** `rustlavel
  new app --with telescope` compiled the dependency in and left `main.rs`
  saying nothing about it — the application ran, `/telescope` answered 404, and
  the generated file gave no hint a line was missing. The scaffold writes the
  line now, for the plugins that can be built from nothing; the ones needing a
  database handle are named in a comment rather than emitted as code that
  would not compile.
- `DebugBar` was not re-exported from the meta-crate at all, and neither it,
  `Metrics` nor `Telescope` was in the prelude — which is the one thing a
  generated `main.rs` imports.
- One OTel test failed a few runs in a hundred for no reason. It asserts that
  a tag byte is absent from an encoded span, and reasoned carefully about the
  identifiers while forgetting that `Span::new` timestamps itself from the
  clock, so the nanoseconds supplied a stray `0x7a` whenever they contained
  one. The times are pinned.

## 0.4.0 — 2026-09-03

A new package, two bugs that made features look like they worked when they did
not, and the administration area the starter kit was missing.

### Added

- **`rustlavel-audit`** — who did what, to which record, from where. Different
  from the application log on purpose: a log line is for whoever is debugging,
  an audit entry is a record somebody may be asked about a year later.
  `req.audit("users.deleted").on("User", id).describe(...).record()`. The
  address and the user agent are read off the request rather than passed in,
  because an entry that says "someone updated the settings" answers nothing.
  The actor column is nullable and deliberately not a foreign key: an entry
  outlives the account it describes, and a cascade would delete the evidence
  along with the subject.
- **RS256 in `rustlavel-webauthn`.** TPM-backed Windows Hello and a number of
  older security keys sign with nothing else, so a relying party that does not
  offer it turns them away at registration — which is what Chrome's
  `pubKeyCredParams` warning is about. The arithmetic is written here, and the
  crypto rule survives intact: verifying an RSA signature involves no secret
  at all. The padding is checked by re-encoding the block a valid signature
  would produce, never by parsing the recovered one — a lenient parser is how
  Bleichenbacher's 2006 forgery worked.
- Menu management and an audit page in the starter kit, plus a brand colour on
  Settings → Appearance that reaches every page. The hue comes from the choice
  and the lightness ladder stays, which is what keeps white-on-brand legible
  when somebody picks an unusual colour.
- `QueryBuilder::or_filter_op` and `or_filter_like`: `or_filter` could only
  ever mean equality, so a search across three columns with `like` had no way
  to say so.
- `Permissions::users_in_role`, which answers "is anybody still holding this
  role" with a count rather than a list.

### Fixed

- **Passkeys could not be registered at all.** The starter kit's script sent an
  invention of its own — `raw_id`, `client_data_json`, flat rather than nested
  under `response` — where the parser reads what
  `PublicKeyCredential.toJSON()` produces, so every credential arrived with no
  `rawId`.
- **Settings → Appearance had never coloured anything.** `/css/theme.css` was
  served as `text/plain`, because `Response::with_text` sets its own content
  type and was chained after the header that set the right one. A browser
  refuses a stylesheet served as plain text and says so only in a console
  warning, so nothing in the application noticed.
- **`Files` sent `cache-control: public, max-age=3600` on everything, with no
  way to change it.** Nothing it serves is fingerprinted, so that is a promise
  the filename cannot keep: rebuild a stylesheet and the browser keeps showing
  the old one for the rest of the hour. The default is now `no-cache` — cache,
  but ask first — and `Files::cache_control` is there for URLs that do carry a
  content hash.
- **`BoxFuture` was not reachable outside `rustlavel-http`**, so a middleware
  written in an application could not spell its own signature. The trait was a
  documented extension point that could not be extended.
- `rustlavel-auth` called `GenericArray::from_slice`, deprecated in newer
  generic-array releases. The framework's lockfile pins an older one, so clippy
  here stayed clean while every freshly scaffolded project got three warnings
  out of a dependency it never asked for.
- The Roles page's Users column had been empty since the page was written: the
  handler filled it with a literal null because nothing in `rustlavel-rbac`
  could answer it.
- Sign-in tab order. "Forgot?" sat between the address and the password in the
  document, so everybody tabbing out of their email landed on it.

### Changed

- Five settings on the starter kit's Security tab were stored and read by
  nothing, which is worse than not having them. Magic-link sign-in, email
  verification, password-reuse prevention, the idle session timeout and the
  From address on the Email tab all do something now. `auth.require_mfa` was
  enforced by the login form and by nothing else, so an activation link, a
  password reset and a magic link were three ways around it.

## 0.3.2 — 2026-09-03

Three bugs that only appear when the thing is run rather than compiled.

### Fixed

- **`FLAGS_ON` and `FLAGS_OFF` did nothing.** The crate read `flags.on` from
  configuration and nothing mapped the environment into it, so the documented
  incident switch was inert. It now reads the environment when the
  configuration key is absent.
- **The starter kit's seeder logged the first administrator's activation link
  at `info`**, so `LOG_LEVEL=warn` hid the only way into a new application. It
  is printed now, with the full URL.
- The four OTLP collector tests share one container and one log, and could
  interleave under a full workspace run. They take turns.
- `docker/env.sh` printed `KEY=value` without `export`, so `eval` set a shell
  variable that `cargo` never saw and every integration suite skipped while
  reporting a pass. It also now sets `DATABASE_URL`, without which fifteen
  more did the same.

## 0.3.1 — 2026-09-03

### Fixed

- `rustlavel new --with rbac` and `--with flags` were refused: both are
  features of the `rustlavel` crate, and neither was in the list the scaffold
  accepts. `rbac` had been missing for a release. A test now reads the
  meta-crate's manifest and fails if the two lists disagree in either
  direction, so this cannot drift again.

## 0.3.0 — 2026-09-03

The minor number, because `Errors` gained a field and a `Validator` built from
a request now behaves differently for a browser: it redirects instead of
rendering text. Everything else is new.

### Changed

- **A failed HTML form now redirects back to itself** with the messages and the
  submitted input, instead of answering a plain `422` with the page gone. JSON
  clients are unaffected and still get Laravel's shape. An application with no
  session middleware falls back to the old plain-text `422`.

### Added

- `rustlavel-flags` — runtime feature switches resolved per user or tenant,
  with percentage rollouts that are stable across processes, a store for
  operator overrides, and `FLAGS_OFF` as an incident switch that is read before
  the store is touched.
- `rustlavel make:crud`, `make:package` and `tinker`.
- A cookbook at
  [advancedynamic.github.io/rustlavel/cookbook.html](https://advancedynamic.github.io/rustlavel/cookbook.html).
- `Flash`, in `rustlavel-http`: somewhere to leave a value for exactly one
  further request. Implemented by the session, used by validation and the view
  layer, so neither has to depend on the other.
- `Request::errors`, `old`, `old_field`, `has_errors` and `previous_url`.
- A nightly CI workflow and a `docker/` set that stand up the eight servers the
  integration suites need — including the TLS certificates, which is why
  database TLS was untested even though CI already had the databases.

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
