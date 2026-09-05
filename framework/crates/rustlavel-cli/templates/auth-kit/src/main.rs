use rustlavel::prelude::*;
use {{crate_name}}::support::settings::Settings;
use {{crate_name}}::support::views;
use {{crate_name}}::modules;
use {{crate_name}}::{database, routes};

#[rustlavel::main]
async fn main() -> Result<()> {
    let app = App::new()?;

    // The database is resolved once and shared: the pool, the roles store and
    // the passkey store are all handles onto it. Read before the builder is
    // consumed, since `config()` borrows it.
    let url = rustlavel::env::env_or("DATABASE_URL", "");
    if url.is_empty() {
        return Err(Error::msg(
            "DATABASE_URL is not set. The starter kit keeps users, roles and sign-in history \
             in a database, so it needs one before it can start.",
        ));
    }
    let db = Database::connect(&url).await?;
    let cache = CacheStore::from_config(app.config())?;
    let mailer = rustlavel::mail::Mail::from_config(app.config())?;
    let rbac = Rbac::from_config(db.clone(), app.config())?;

    // The words on the page. `lang/en.json` is the source; anything
    // untranslated falls back to English rather than going blank. Built in
    // `support::views` so the tests build the same one — see the note there.
    let translator = views::translator(app.config())?;
    let views = views::engine(app.config(), app.root(), &translator);
    // The audit trail: who did what, to which record, from where. Registered
    // as a plugin so `req.audit(...)` can find it from any handler.
    let audit = rustlavel::audit::Audit::new(db.clone());
    // What the Settings page writes and everything else reads. Shares one cache
    // across the process, invalidated whenever a setting is saved.
    let settings = Settings::from_config(db.clone(), app.config())?;

    // Sessions on disk rather than in memory, so a restart does not sign
    // everybody out. Swap the store for Redis when there is more than one
    // process, since a session written by one is invisible to the others.
    let sessions = SessionManager::from_config(
        app.config(),
        FileStore::new("storage/sessions"),
    )?;

    // The chain breaks here rather than running to `.run()`, because the
    // modules are a list and a list needs a loop. Everything above the loop is
    // what this application is; everything below it is how it is served.
    let mut app = app
        .state(db.clone())
        // The cache backs the per-address half of the sign-in lockout.
        .state(cache)
        .state(mailer)
        .state(settings)
        // Handlers that translate outside a template — a flash message, a
        // validation error — resolve it from here.
        .state(translator)
        .views(views)
        // Roles and permissions. `req.can(...)` and the `Can` guard both
        // resolve the store from here, and fail closed if it is missing.
        .plugin(rbac)
        .plugin(audit)
{{plugins}}        ;

    // Each feature registers its own routes, middleware and state. `all()` is a
    // hand-written list in `src/modules/mod.rs` — a module that registered
    // itself by existing would be a module nobody can find the registration
    // for.
    for module in modules::all() {
        app = app.plugin_boxed(module);
    }

    app
        // Order matters: the session has to exist before anything reads a
        // login out of it, and the CSRF check reads the session.
        .middleware(sessions)
        .middleware(Csrf::new())
        .routes(routes::auth::routes)
        .routes(routes::web::routes)
        // The built-in migrations, then whatever the modules own.
        .migrations(
            database::migrations::all()
                .into_iter()
                .chain(modules::migrations())
                .collect(),
        )
        .seeders(
            database::seeders::all().into_iter().chain(modules::seeders()).collect(),
        )
        .run()
        .await
}
