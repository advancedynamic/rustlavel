use blog::{database, routes};
use rustlavel::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    let app = App::new()?
        .routes(routes::web::routes)
        .migrations(database::migrations::all())
        .seeders(database::seeders::all());

    // The database is optional at boot: an unreachable one should be reported
    // by `rustlavel doctor`, not by a panic during startup.
    let app = match connect().await {
        Ok(db) => app.state(db),
        Err(error) => {
            warn!("starting without a database: {error}");
            app
        }
    };

    app.run().await
}

async fn connect() -> Result<Database> {
    let url = rustlavel::env::env_or("DATABASE_URL", "");
    if url.is_empty() {
        return Err(Error::msg("DATABASE_URL is not set"));
    }
    Database::connect(&url).await
}
