use rustlavel::prelude::*;
// The *database* `BoxFuture`, which carries a lifetime. The prelude brings in
// the HTTP alias of the same name, which does not — two types, one name, and
// the compiler's message about it names neither.
use rustlavel::db::driver::BoxFuture;

/// One client, so the gateway can introspect on the day it is installed.
///
/// The secret is read from the environment rather than invented here: a seeder
/// that generates one prints it once and it is lost, and a seeder that hardcodes
/// one puts a credential in the repository.
pub struct FirstClientSeeder;

impl Seeder for FirstClientSeeder {
    fn name(&self) -> &'static str {
        "first_client"
    }

    fn run<'a>(&'a self, db: &'a Database) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let id = rustlavel::env::env_or("INTROSPECT_CLIENT_ID", "");
            let secret = rustlavel::env::env_or("INTROSPECT_CLIENT_SECRET", "");
            if id.is_empty() || secret.is_empty() {
                warn!(
                    "INTROSPECT_CLIENT_ID and INTROSPECT_CLIENT_SECRET are not set, so no client \
                     was created. The resource server cannot introspect until one exists."
                );
                return Ok(());
            }

            // Safe to run twice, like every seeder in this project: re-running
            // after adding a client fills the gap rather than failing half-way.
            if db.table("oauth_clients").filter("client_id", id.as_str()).exists(db).await? {
                return Ok(());
            }

            db.table("oauth_clients")
                .insert(
                    db,
                    &[
                        ("client_id", id.as_str().into()),
                        ("secret_hash", rustlavel::auth::hash_password(&secret)?.into()),
                        ("name", "Introspection".into()),
                        ("redirect_uris", "".into()),
                        ("scopes", "".into()),
                    ],
                )
                .await?;
            Ok(())
        })
    }
}
