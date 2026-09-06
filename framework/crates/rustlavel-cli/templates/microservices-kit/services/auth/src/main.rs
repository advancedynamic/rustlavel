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

#[path = "../database/migrations/mod.rs"]
mod migrations;
#[path = "../database/seeders/mod.rs"]
mod seeders;

#[rustlavel::main]
async fn main() -> Result<()> {
    let app = App::new()?;

    let url = rustlavel::env::env_or("DATABASE_URL", "");
    if url.is_empty() {
        return Err(Error::msg(
            "DATABASE_URL is not set. The authorization server keeps users, clients and tokens \
             in its own database, separate from the resource server's.",
        ));
    }
    let db = Database::connect(&url).await?;

    app.state(db.clone())
        .migrations(migrations::all())
        .seeders(seeders::all())
        .run()
        .await
}
