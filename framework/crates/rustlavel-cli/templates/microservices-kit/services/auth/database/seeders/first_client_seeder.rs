use rustlavel::prelude::*;
// The *database* `BoxFuture`, which carries a lifetime. The prelude brings in
// the HTTP alias of the same name, which does not — two types, one name, and
// the compiler's message about it names neither.
use rustlavel::db::driver::BoxFuture;

/// The two clients a fresh installation needs, and why they are two.
///
/// **They must not be one.** The introspection client's secret lives in the
/// resource server's `.env`, because that is the service which calls
/// `/oauth/introspect`. Give that same credential the scopes the domain checks
/// and anybody who reads that file can mint a token that writes orders — a
/// privilege escalation out of a configuration file. So the introspection
/// client carries no scopes at all: it may ask questions about tokens and may
/// not obtain a useful one.
///
/// **Secrets are generated when none is set, and printed once.** The first
/// version of this refused to act and warned instead, on the grounds that a
/// generated secret is printed once and lost — which is true, and left a fresh
/// scaffold with no client at all. `db:seed` produced a cheerful message and a
/// system that could not introspect, and the only sign was a line of yellow
/// text among the migrations. Printing the `.env` lines to paste answers the
/// objection: it is lost only if it is ignored.
///
/// A secret is never hardcoded and never written to a file by this seeder. The
/// database holds the digest, the way it holds a password hash.
pub struct FirstClientSeeder;

/// What the resource server's own routes check. Change these with the scopes
/// your domain actually uses — and keep them off the introspection client.
const DOMAIN_SCOPES: &str = "orders.read orders.write";

impl Seeder for FirstClientSeeder {
    fn name(&self) -> &'static str {
        "first_client"
    }

    fn run<'a>(&'a self, db: &'a Database) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // Introspection: no scopes. It asks about tokens; it does not carry
            // authority of its own.
            let introspect_id = match rustlavel::env::env_or("INTROSPECT_CLIENT_ID", "").trim() {
                "" => "introspection".to_string(),
                set => set.to_string(),
            };
            let introspect_secret = rustlavel::env::env_or("INTROSPECT_CLIENT_SECRET", "");

            if let Some(secret) =
                create(db, &introspect_id, &introspect_secret, "Introspection", "").await?
            {
                println!(
                    "\n  A client was created for introspection. Put these in .env — the secret \
                     is not stored and will not be shown again:\n\n    \
                     INTROSPECT_CLIENT_ID={introspect_id}\n    \
                     INTROSPECT_CLIENT_SECRET={secret}\n"
                );
            }

            // And one that can actually obtain a token, so the installation can
            // be exercised the day it is installed. Without it `db:seed`
            // finishes, everything looks healthy, and the first token request
            // comes back `invalid_scope` with nothing in the project to explain
            // it — the client it gave you was never allowed any.
            let demo_id = match rustlavel::env::env_or("DEMO_CLIENT_ID", "").trim() {
                "" => "demo".to_string(),
                set => set.to_string(),
            };
            let demo_secret = rustlavel::env::env_or("DEMO_CLIENT_SECRET", "");

            if let Some(secret) =
                create(db, &demo_id, &demo_secret, "Demo", DOMAIN_SCOPES).await?
            {
                println!(
                    "  And one that may call the API, for trying it out. Delete it before this \
                     reaches anywhere real:\n\n    DEMO_CLIENT_ID={demo_id}\n    \
                     DEMO_CLIENT_SECRET={secret}\n    scopes: {DOMAIN_SCOPES}\n"
                );
            }

            Ok(())
        })
    }
}

/// Create a client, returning the generated secret when one was generated.
///
/// `None` means nothing was printed: either the client already existed, or the
/// operator supplied the secret and already has it. Printing a secret that was
/// not stored would hand somebody a value authenticating nothing, which they
/// would spend an afternoon on.
async fn create(
    db: &Database,
    id: &str,
    configured: &str,
    name: &str,
    scopes: &str,
) -> Result<Option<String>> {
    // Safe to run twice, like every seeder in this project: re-running after
    // adding a client fills the gap rather than failing half-way.
    if db.table("oauth_clients").filter("client_id", id).exists(db).await? {
        info!("the client `{id}` already exists; its secret is unchanged");
        return Ok(None);
    }

    let generated = configured.trim().is_empty();
    // 32 bytes of hex. Long enough that guessing is not a strategy, and shaped
    // so it can be pasted into a file without quoting.
    let secret = match generated {
        true => rustlavel::auth::random::hex(32),
        false => configured.to_string(),
    };

    db.table("oauth_clients")
        .insert(
            db,
            &[
                ("client_id", id.into()),
                // **The provider's own digest, not `hash_password`.** This used
                // to store an argon2 hash, which the authorization server never
                // compares against: it digests client secrets with SHA-256 on
                // purpose — they are 256 bits from the CSPRNG, so there is
                // nothing to guess, and argon2's deliberate cost on the token
                // endpoint is a denial of service an attacker triggers by
                // sending requests. The two hashes never matched, so a seeded
                // client could not authenticate once.
                ("secret_hash", rustlavel::oauth_provider::store::digest(&secret).into()),
                ("name", name.into()),
                ("redirect_uris", "".into()),
                ("scopes", scopes.into()),
            ],
        )
        .await?;

    Ok(generated.then_some(secret))
}
