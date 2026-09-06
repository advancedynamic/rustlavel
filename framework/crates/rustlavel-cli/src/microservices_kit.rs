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
    ("Cargo.toml", include_str!("../templates/microservices-kit/Cargo.toml")),
    ("shared/Cargo.toml", include_str!("../templates/microservices-kit/shared/Cargo.toml")),
    ("shared/src/lib.rs", include_str!("../templates/microservices-kit/shared/src/lib.rs")),
    (
        "services/gateway/Cargo.toml",
        include_str!("../templates/microservices-kit/services/gateway/Cargo.toml"),
    ),
    (
        "services/gateway/src/main.rs",
        include_str!("../templates/microservices-kit/services/gateway/src/main.rs"),
    ),
    (
        "services/auth/Cargo.toml",
        include_str!("../templates/microservices-kit/services/auth/Cargo.toml"),
    ),
    (
        "services/auth/src/main.rs",
        include_str!("../templates/microservices-kit/services/auth/src/main.rs"),
    ),
    (
        "services/api/Cargo.toml",
        include_str!("../templates/microservices-kit/services/api/Cargo.toml"),
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
GATEWAY_PORT=9000
AUTH_URL=http://127.0.0.1:9001
API_URL=http://127.0.0.1:9002

# --- Databases, one per service ---------------------------------------------
# The authorization server owns users, clients, codes and tokens. The resource
# server owns its domain and never reads the other's schema — a second service
# reading it would be a second service to redeploy when it changes.
AUTH_DATABASE_URL=postgres://localhost/{{crate_name}}_auth
API_DATABASE_URL=postgres://localhost/{{crate_name}}_api

# --- How the resource server checks a token ---------------------------------
# `introspect` asks the authorization server (RFC 7662) and caches the answer
# briefly: always current, at the cost of a hop and of depending on that server.
# `signed` verifies an ES256 signature locally: no hop, no dependency, and
# revocation only takes effect when the token expires.
TOKEN_VERIFICATION=introspect
INTROSPECT_CLIENT_ID=
INTROSPECT_CLIENT_SECRET=
# Base64, the SEC1 point the authorization server publishes. Only for `signed`.
TOKEN_PUBLIC_KEY=
"#;

/// The packages the generated code imports.
///
/// Read by `new` when it expands `--with microservices-kit`, so a package the
/// kit needs cannot be forgotten from the dependency list — the same guard the
/// auth kit has.
pub const REQUIRED_PACKAGES: &[&str] =
    &["auth", "cache", "client", "db", "gateway", "oauth", "oauth-provider", "rbac"];
