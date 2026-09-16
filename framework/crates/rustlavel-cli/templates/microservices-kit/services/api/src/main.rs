//! The resource server: it holds the domain and answers for it.
//!
//! **It checks tokens itself.** The gateway in front already refuses a request
//! with no credential, and that saves a hop and nothing else — a gateway is not
//! the only door. Anything on the network can reach this service directly, and
//! a check that happens only at the edge is missing the moment somebody adds a
//! second way in: a cron job, a sidecar, a second gateway, a developer with a
//! port forward.
//!
//! **It owns its own database** and never reads the authorization server's. It
//! knows a subject id and what that subject may do; it does not know what a user
//! row looks like.

use rustlavel::prelude::*;

mod orders;
mod tokens;

#[path = "../database/migrations/mod.rs"]
mod migrations;

// Re-exported so `orders.rs` names them once rather than repeating the shared
// crate's name on every line.
pub use {{crate_name}}_shared::{Caller, problem};

#[rustlavel::main]
async fn main() -> Result<()> {
    let app = App::new()?;

    // `API_DATABASE_URL`, not `DATABASE_URL`. Each service owns its own database, so
    // one shared key would be one shared schema — the coupling this kit is
    // shaped to avoid, arriving through a configuration file.
    let url = rustlavel::env::env_or("API_DATABASE_URL", "");
    if url.is_empty() {
        return Err(Error::msg(
            "API_DATABASE_URL is not set. The resource server keeps its domain in its own database, \
             separate from the authorization server's.",
        ));
    }
    let db = Database::connect(&url).await?;
    let cache = CacheStore::from_config(app.config())?;

    // Announced to the registry, when there is one. Held for the life of the
    // process: dropping the registrar would stop the heartbeats and the
    // registry would conclude this service died.
    let registrar = {{crate_name}}_shared::join("api", app.config());

    app.on_shutdown({{crate_name}}_shared::farewell(registrar))
        .state(db.clone())
        .state(cache)
        // How a token is checked is a deployment decision, not a code one: see
        // `tokens.rs`. Both ways answer with the same `Caller`, so nothing
        // below this line knows which was used.
        .state(tokens::Verifier::from_env())
        .middleware(tokens::RequireCaller)
        .migrations(migrations::all())
        .routes(routes)
        .run()
        .await
}

fn routes(r: &mut Router) {
    // Open: a load balancer and the gateway's aggregate check both ask for it,
    // and neither carries a token.
    r.get("/health", |_req: Request| async { Json::object([("status", Json::from("up"))]) });

    r.get("/api/me", |req: Request| async move {
        match req.extension::<Caller>() {
            Some(caller) => Response::json(caller.to_json()),
            None => Response::new(Status::UNAUTHORIZED).with_json(problem("Unauthenticated.")),
        }
    });

    // The domain this service owns. `/api/me` shows the token plumbing; this
    // shows the shape of a service — and it is what reads the database the
    // service opened at boot.
    orders::routes(r);
}
