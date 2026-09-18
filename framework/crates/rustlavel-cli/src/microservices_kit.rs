//! The `microservices-kit` scaffold: a gateway, an authorization server and a
//! resource server, as a cargo workspace.
//!
//! **A workspace, not one crate with three `[[bin]]` entries.** Each service
//! builds and deploys on its own, and — more to the point — they cannot quietly
//! share a model. Sharing types between services is the coupling microservices
//! exist to avoid, so what crosses lives in `shared/` where somebody has to look
//! at it to add to it.
//!
//! **Two of the three were already written.** `rustlavel-oauth-provider` is a
//! complete OAuth 2.1 authorization server, and `RequireToken` with
//! `rustlavel-rbac` is the resource-server half. This kit wires them up; only
//! the gateway needed a crate of its own, because nothing in the framework
//! forwarded a request until `rustlavel-gateway`.
//!
//! Like [`crate::auth_kit`], every file the scaffold writes is a template file
//! in one manifest and goes out through the placeholder renderer. Nothing is
//! written from a constant: that split is what let 0.7.0 ship a seeder still
//! carrying `{{crate_name}}`.

/// Every file, rendered on the way out.
pub const FILES: &[(&str, &str)] = &[
    // The ordinary scaffold's README is skipped for this kit — it describes an
    // application, and this is four of them — so the kit writes its own. Four
    // services, two databases and three setup steps is too much to leave to the
    // comments in a .env nobody opens first.
    ("README.md", include_str!("../templates/microservices-kit/README.md")),
    ("Cargo.toml", include_str!("../templates/microservices-kit/Cargo.toml.template")),
    ("shared/Cargo.toml", include_str!("../templates/microservices-kit/shared/Cargo.toml.template")),
    ("shared/src/lib.rs", include_str!("../templates/microservices-kit/shared/src/lib.rs")),
    (
        "services/gateway/Cargo.toml",
        include_str!("../templates/microservices-kit/services/gateway/Cargo.toml.template"),
    ),
    (
        "services/gateway/src/main.rs",
        include_str!("../templates/microservices-kit/services/gateway/src/main.rs"),
    ),
    (
        "services/auth/Cargo.toml",
        include_str!("../templates/microservices-kit/services/auth/Cargo.toml.template"),
    ),
    (
        "services/auth/src/main.rs",
        include_str!("../templates/microservices-kit/services/auth/src/main.rs"),
    ),
    (
        "services/api/Cargo.toml",
        include_str!("../templates/microservices-kit/services/api/Cargo.toml.template"),
    ),
    (
        "services/api/src/main.rs",
        include_str!("../templates/microservices-kit/services/api/src/main.rs"),
    ),
    (
        "services/api/src/tokens.rs",
        include_str!("../templates/microservices-kit/services/api/src/tokens.rs"),
    ),
    (
        "services/auth/database/migrations/mod.rs",
        include_str!("../templates/microservices-kit/services/auth/database/migrations/mod.rs"),
    ),
    (
        "services/auth/database/migrations/2026_09_06_000100_create_oauth_tables.rs",
        include_str!("../templates/microservices-kit/services/auth/database/migrations/2026_09_06_000100_create_oauth_tables.rs"),
    ),
    (
        "services/auth/database/seeders/mod.rs",
        include_str!("../templates/microservices-kit/services/auth/database/seeders/mod.rs"),
    ),
    (
        "services/api/src/orders.rs",
        include_str!("../templates/microservices-kit/services/api/src/orders.rs"),
    ),
    (
        "services/api/database/migrations/mod.rs",
        include_str!("../templates/microservices-kit/services/api/database/migrations/mod.rs"),
    ),
    (
        "services/api/database/migrations/2026_09_16_000100_create_orders_table.rs",
        include_str!("../templates/microservices-kit/services/api/database/migrations/2026_09_16_000100_create_orders_table.rs"),
    ),
    (
        "services/registry/Cargo.toml",
        include_str!("../templates/microservices-kit/services/registry/Cargo.toml.template"),
    ),
    (
        "services/registry/src/main.rs",
        include_str!("../templates/microservices-kit/services/registry/src/main.rs"),
    ),
    (
        "services/auth/database/seeders/first_client_seeder.rs",
        include_str!("../templates/microservices-kit/services/auth/database/seeders/first_client_seeder.rs"),
    ),
];

/// Appended to `.env`.
///
/// Three ports and **two databases**. The gateway has none on purpose: the
/// moment it owns a table it stops being a gateway and becomes a fourth service
/// that can fail — the one every request already passes through.
pub const ENV_ADDITIONS: &str = r#"
# --- Services ---------------------------------------------------------------
# Four services share this file, so the port cannot live in it: they would all
# read the same `SERVER_PORT` and fight over one socket. A real environment
# variable beats anything here, which is what makes the lines below work:
#
#   SERVER_PORT=8761 cargo run -p {{crate_name}}_registry
#   SERVER_PORT=9001 cargo run -p {{crate_name}}_auth
#   SERVER_PORT=9002 cargo run -p {{crate_name}}_api
#   SERVER_PORT=9000 cargo run -p {{crate_name}}_gateway
#
# Where the gateway sends what it receives, unless DISCOVERY_URL is set below.
AUTH_URL=http://127.0.0.1:9001
API_URL=http://127.0.0.1:9002

# --- Service discovery, optional --------------------------------------------
# Blank means the fixed addresses above are used, which is right on a platform
# that already tracks instances — and right for one of each on a laptop. Set it
# and the gateway resolves `auth` and `api` per request instead, and each
# service announces itself on the way up.
#
# SERVICE_HOST must be an address *other machines* can reach: the registry hands
# out what it is told, so a service that registers 127.0.0.1 has told the whole
# estate to talk to itself.
DISCOVERY_URL=
SERVICE_HOST=127.0.0.1
ZONE=default
# Every registry node including this one, comma separated. Each node drops its
# own address and forwards writes to the rest.
REGISTRY_URL=http://127.0.0.1:8761
PEERS=

# --- Databases, one per service ---------------------------------------------
# The authorization server owns users, clients, codes and tokens. The resource
# server owns its domain and never reads the other's schema — a second service
# reading it would be a second service to redeploy when it changes.
AUTH_DATABASE_URL=postgres://localhost/{{crate_name}}_auth
API_DATABASE_URL=postgres://localhost/{{crate_name}}_api

# --- How the resource server checks a token ---------------------------------
# `introspect` asks the authorization server (RFC 7662) and caches the answer
# for thirty seconds, so a revoked token keeps working for up to that long —
# measured, not assumed. It costs a hop and makes this service depend on that
# one. `signed` verifies an ES256 signature locally: no hop, no dependency, and
# revocation only takes effect when the token expires, which is an hour.
#
# So the choice is thirty seconds against an hour, not "immediate" against an
# hour. Shorten INTROSPECTION_TTL in the crate if thirty seconds is too long for
# what you are protecting; the cost is a request to the authorization server per
# token per window.
TOKEN_VERIFICATION=introspect
INTROSPECT_CLIENT_ID=
INTROSPECT_CLIENT_SECRET=
# Base64, the SEC1 point the authorization server publishes. Only for `signed`.
TOKEN_PUBLIC_KEY=

# --- A client for trying it out ---------------------------------------------
# Seeded with the scopes the resource server checks, so the installation can be
# exercised on the day it is installed. Kept separate from the introspection
# credential above on purpose: that one lives in the resource server's own
# configuration, and giving it domain scopes would let anybody who reads that
# file mint a token that writes. Delete this client before this reaches
# anywhere real.
DEMO_CLIENT_ID=
DEMO_CLIENT_SECRET=
"#;

/// The packages the generated code imports.
///
/// Read by `new` when it expands `--with microservices-kit`, so a package the
/// kit needs cannot be forgotten from the dependency list — the same guard the
/// auth kit has.
pub const REQUIRED_PACKAGES: &[&str] =
    &["auth", "cache", "client", "db", "discovery", "gateway", "oauth", "oauth-provider", "rbac"];
