//! The authorization server: it issues tokens and answers questions about them.
//!
//! Almost none of this is written here. `rustlavel-oauth-provider` is a
//! complete OAuth 2.1 authorization server — `/oauth/token`, `/oauth/revoke`,
//! `/oauth/introspect` (RFC 7662) and the RFC 8414 metadata document — so this
//! service configures it and gets out of the way.
//!
//! **It owns its own database**, and it is the only service that reads it. Users,
//! clients, authorization codes and tokens live here and nowhere else. A second
//! service reading this schema would be a second service that has to be
//! redeployed when it changes.

use rustlavel::prelude::*;
use rustlavel::oauth_provider::database::{
    DatabaseClientStore, DatabaseCodeStore, DatabaseConsentStore, DatabaseTokenStore,
};
use rustlavel::oauth_provider::{AuthorizationServer, OAuthProvider};

#[path = "../database/migrations/mod.rs"]
mod migrations;
#[path = "../database/seeders/mod.rs"]
mod seeders;

#[rustlavel::main]
async fn main() -> Result<()> {
    let app = App::new()?;

    // `AUTH_DATABASE_URL`, not `DATABASE_URL`. Each service owns its own database, so
    // one shared key would be one shared schema — the coupling this kit is
    // shaped to avoid, arriving through a configuration file.
    let url = rustlavel::env::env_or("AUTH_DATABASE_URL", "");
    if url.is_empty() {
        return Err(Error::msg(
            "AUTH_DATABASE_URL is not set. The authorization server keeps users, clients and tokens \
             in its own database, separate from the resource server's.",
        ));
    }
    let db = Database::connect(&url).await?;

    // Announced to the registry, when there is one. Held for the life of the
    // process: dropping the registrar would stop the heartbeats.
    let registrar = {{crate_name}}_shared::join("auth", app.config());

    // **The line that makes this an authorization server.** Without it the
    // service migrates three OAuth tables, seeds a client, and serves no token
    // endpoint at all — `/oauth/token` answers 404, and the file above says in
    // its own first paragraph that the provider is mounted here. Found by
    // asking it for a token.
    // **Every store backed by a table.** The in-memory ones are right for a
    // test and for a development server and wrong for anything that restarts:
    // an empty store answers "unknown token" to everything, so a deploy would
    // sign out every session and break every integration at once.
    let server = AuthorizationServer::new(DatabaseClientStore::new(db.clone()))
        .storing_codes(DatabaseCodeStore::new(db.clone()))
        .storing_tokens(DatabaseTokenStore::new(db.clone()))
        .storing_consent(DatabaseConsentStore::new(db.clone()))
        .configured(app.config());

    app.on_shutdown({{crate_name}}_shared::farewell(registrar))
        .state(db.clone())
        .plugin(OAuthProvider::new(server))
        .migrations(migrations::all())
        .seeders(seeders::all())
        .routes(routes)
        .run()
        .await
}

fn routes(r: &mut Router) {
    // **Every service needs this, including this one.** Without it the
    // gateway's aggregate check probes `/health` here, gets a 404, and reports
    // the whole system `degraded` with a 503 — on a deployment where nothing is
    // wrong. A load balancer reading that takes the gateway out of rotation,
    // and the one endpoint that could have explained it is the one that said so.
    //
    // Open, and it must be: the gateway and the load balancer both ask for it
    // and neither carries a token.
    r.get("/health", |_req: Request| async { Json::object([("status", Json::from("up"))]) });
}
