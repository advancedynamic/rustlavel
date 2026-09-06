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

mod tokens;

#[rustlavel::main]
async fn main() -> Result<()> {
    let app = App::new()?;

    let url = rustlavel::env::env_or("DATABASE_URL", "");
    if url.is_empty() {
        return Err(Error::msg(
            "DATABASE_URL is not set. The resource server keeps its domain in its own database, \
             separate from the authorization server's.",
        ));
    }
    let db = Database::connect(&url).await?;
    let cache = CacheStore::from_config(app.config())?;

    app.state(db.clone())
        .state(cache)
        // How a token is checked is a deployment decision, not a code one: see
        // `tokens.rs`. Both ways answer with the same `Caller`, so nothing
        // below this line knows which was used.
        .state(tokens::Verifier::from_env())
        .middleware(tokens::RequireCaller)
        .routes(routes)
        .run()
        .await
}

fn routes(r: &mut Router) {
    // Open: a load balancer and the gateway's aggregate check both ask for it,
    // and neither carries a token.
    r.get("/health", |_req: Request| async { Json::object([("status", Json::from("up"))]) });

    r.get("/api/me", |req: Request| async move {
        match req.extension::<{{crate_name}}_shared::Caller>() {
            Some(caller) => Response::json(caller.to_json()),
            None => Response::new(Status::UNAUTHORIZED)
                .with_json({{crate_name}}_shared::problem("Unauthenticated.")),
        }
    });
}
