//! `rustlavel new <app>` — scaffold an application.
//!
//! Deliberately slim, following Laravel 11+: routes, one controller, config,
//! public, tests. Everything else arrives when a package is added.

use crate::auth_kit;
use crate::naming;
use crate::stubs::{self, render};
use crate::console;
use crate::ask;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The packages `--with` can turn on, and what each one needs on disk.
const PACKAGES: &[(&str, &[&str])] = &[
    ("ai", &[]),
    // Not a crate of its own: `auth-kit` writes a working sign-in, roles and
    // an administration area into the project, and turns on the packages that
    // code needs.
    ("audit", &["database/migrations"]),
    ("auth", &["storage/sessions"]),
    ("auth-kit", &["storage/sessions", "resources/views", "public/css", "public/js"]),
    ("cache", &["storage/cache"]),
    ("client", &[]),
    ("db", &["database/migrations", "database/seeders"]),
    ("debugbar", &[]),
    ("flags", &[]),
    ("i18n", &["lang"]),
    ("ldap", &[]),
    ("mail", &["resources/views"]),
    ("mcp", &[]),
    ("metrics", &[]),
    ("model-cache", &[]),
    ("oauth", &["storage/sessions"]),
    ("oauth-provider", &["storage/sessions"]),
    ("openapi", &[]),
    ("otel", &[]),
    ("queue", &["database/migrations"]),
    ("rbac", &["database/migrations"]),
    ("search", &[]),
    ("storage", &["storage/app"]),
    ("telescope", &[]),
    ("validation", &[]),
    ("vault", &[]),
    ("view", &["resources/views"]),
    ("webauthn", &["storage/sessions"]),
    ("ws", &[]),
];

pub fn run(args: &[String]) -> Result<(), String> {
    let mut name = None;
    let mut local_framework: Option<String> = None;
    let mut packages: Vec<String> = Vec::new();
    let mut chose_packages = false;
    let mut ask_nothing = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // Point the new app at a framework checkout instead of crates.io,
            // which is what the framework's own smoke tests need.
            "--local" => {
                local_framework =
                    Some(iter.next().ok_or("--local needs a path to the framework workspace")?.clone())
            }
            // `rustlavel new blog --with db,view` is the scaffold-time form of
            // `cargo add`: the same opt-in, chosen up front.
            "--with" => {
                let list = iter.next().ok_or("--with needs a comma-separated list of packages")?;
                for requested in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    if !PACKAGES.iter().any(|(known, _)| *known == requested) {
                        return Err(format!(
                            "`{requested}` is not a package. Available: {}",
                            PACKAGES.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(", ")
                        ));
                    }
                    packages.push(requested.to_string());
                }
                chose_packages = true;
            }
            "--all" => {
                packages = PACKAGES.iter().map(|(name, _)| (*name).to_string()).collect();
                chose_packages = true;
            }
            // For a script, and for anybody who would rather not be asked.
            "--yes" | "-y" | "--no-input" => ask_nothing = true,
            other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
            other => name = Some(other.to_string()),
        }
    }

    // Asked for rather than refused, when there is somebody to ask. A person
    // who typed `rustlavel new` and meant it should not have to read a usage
    // line to find out the command wanted one more word.
    let name = match name {
        Some(given) => given,
        None if ask::interactive() && !ask_nothing => {
            let given = ask::text("What is the project called?", "app");
            if given.trim().is_empty() {
                return Err("a project needs a name.".into());
            }
            given.trim().to_string()
        }
        None => {
            return Err(
                "usage: rustlavel new <name> [--with db,view] [--all] [--yes] [--local <path>]"
                    .into(),
            );
        }
    };

    // Nothing was asked for, and there is somebody there to ask. Everything
    // below this point is the same whether the answers came from the flags or
    // from the questions.
    if !chose_packages && !ask_nothing {
        packages = interview();
    }
    // Kept before `auth-kit` is expanded into the nine packages it implies, so
    // the summary shows what somebody chose rather than what that turned into.
    let chosen = packages.clone();

    // `auth-kit` is scaffolding rather than a feature flag, so it is expanded
    // into the packages its generated code actually imports and then dropped
    // from the list passed to Cargo.
    let auth_kit = packages.iter().any(|p| p == "auth-kit");
    if auth_kit {
        for required in
            ["audit", "auth", "db", "view", "validation", "rbac", "webauthn", "cache", "mail"]
        {
            packages.push(required.to_string());
        }
        packages.retain(|p| p != "auth-kit");
    }

    packages.sort();
    packages.dedup();

    let crate_name = naming::snake(&name);
    let root = PathBuf::from(&name);

    if root.exists() {
        return Err(format!("`{name}` already exists"));
    }

    let source = match &local_framework {
        Some(path) => {
            let absolute = std::fs::canonicalize(path)
                .map_err(|e| format!("cannot resolve --local path `{path}`: {e}"))?;
            format!("path = \"{}/crates/rustlavel\"", absolute.display())
        }
        None => format!("version = \"{}\"", env!("CARGO_PKG_VERSION")),
    };

    let dependency = if packages.is_empty() {
        source
    } else {
        let list = packages.iter().map(|p| format!("\"{p}\"")).collect::<Vec<_>>().join(", ");
        format!("{source}, features = [{list}]")
    };

    let mut values = BTreeMap::new();
    // `auth-kit` writes its own `main.rs`, and that file already registers two
    // of these — naming them again as owed would tell somebody to add a line
    // that is four lines above.
    let already: &[&str] = if auth_kit { &["rbac", "audit"] } else { &[] };
    values.insert("plugins", plugin_lines(&packages, already));
    // A `DATABASE_URL` for the engine that was chosen, so somebody who said
    // "MySQL" is not handed a PostgreSQL line to correct. Blank when nothing
    // in this project uses a database, rather than a commented-out URL that
    // looks like a setting somebody turned off.
    values.insert(
        "database",
        match packages.iter().any(|p| p == "db") {
            true => format!("\n{}\n", database_url_line()),
            false => String::new(),
        },
    );
    values.insert("name", crate_name.clone());
    values.insert("crate_name", crate_name.clone());
    values.insert("app_name", naming::pascal(&name));
    values.insert("dependency", dependency);

    // The last chance to say no, and it only appears when the answers came
    // from questions: somebody who typed `--with db,view` has already said
    // what they want and does not need to say it twice.
    if !chose_packages && !ask_nothing && ask::interactive() {
        let summary = match chosen.is_empty() {
            true => "no packages".to_string(),
            false => chosen.join(", "),
        };
        println!();
        console::info(&format!("{} with {summary}", console::bold(&name)));
        if !ask::confirm("Create it?", true) {
            console::info("Nothing was written.");
            return Ok(());
        }
    }

    console::heading(&format!("Creating {}", console::accent(&name)));

    let files: &[(&str, &str)] = &[
        ("Cargo.toml", stubs::CARGO_TOML),
        (".gitignore", stubs::GITIGNORE),
        (".env", stubs::ENV),
        (".env.example", stubs::ENV),
        ("README.md", stubs::README),
        ("CLAUDE.md", stubs::AGENT_NOTES),
        ("config/app.json", stubs::CONFIG_APP),
        ("public/README.md", stubs::PUBLIC_KEEP),
        (
            "src/main.rs",
            // With the database package, main.rs also registers the generated
            // migration and seeder lists.
            if packages.iter().any(|p| p == "db") { stubs::MAIN_RS_DB } else { stubs::MAIN_RS },
        ),
        ("src/routes/mod.rs", stubs::ROUTES_MOD),
        ("src/routes/web.rs", stubs::ROUTES_WEB),
        ("src/controllers/mod.rs", stubs::CONTROLLERS_MOD),
        ("src/controllers/welcome_controller.rs", stubs::WELCOME_CONTROLLER),
        ("tests/web.rs", stubs::TEST_STUB),
    ];

    for (path, template) in files {
        write(&root.join(path), &render(template, &values))?;
        console::created(path);
    }

    // `tests/web.rs` reaches into the app's modules, which requires a library
    // target beside the binary.
    let lib = if packages.iter().any(|p| p == "db") {
        format!("{LIB_RS}pub mod database;\n")
    } else {
        LIB_RS.to_string()
    };
    write(&root.join("src/lib.rs"), &lib)?;
    console::created("src/lib.rs");

    // The database package needs its generated registries to exist before the
    // first build, since main.rs names them.
    if packages.iter().any(|p| p == "db") {
        let empty: BTreeMap<&str, String> =
            [("modules", String::new()), ("entries", String::new())].into_iter().collect();

        write(
            &root.join("database/migrations/mod.rs"),
            &render(stubs::MIGRATIONS_REGISTRY, &empty),
        )?;
        write(&root.join("database/seeders/mod.rs"), &render(stubs::SEEDERS_REGISTRY, &empty))?;
        write(&root.join("src/database.rs"), DATABASE_BRIDGE)?;
        console::created("database/migrations/mod.rs");
        console::created("database/seeders/mod.rs");
        console::created("src/database.rs");
    }

    // A package that needs a directory gets one now, so the first run does not
    // fail on a path that was only ever going to be created by hand.
    for (package, directories) in PACKAGES {
        if !packages.iter().any(|enabled| enabled == package) {
            continue;
        }
        for directory in *directories {
            std::fs::create_dir_all(root.join(directory))
                .map_err(|e| format!("cannot create {directory}: {e}"))?;
            write(&root.join(directory).join(".gitkeep"), "")?;
            console::created(directory);
        }
    }

    // The mail package reads `mail.*` from configuration, and configuration
    // only knows what a file under `config/` declares. Without this the whole
    // `MAIL_*` block in `.env` is inert: the mailer uses its defaults while the
    // settings screen, which reads the environment directly, reports that the
    // environment is in charge.
    if packages.iter().any(|p| p == "mail") {
        write(&root.join("config/mail.json"), stubs::CONFIG_MAIL)?;
        console::created("config/mail.json");

        for file in [".env", ".env.example"] {
            let path = root.join(file);
            let mut contents = std::fs::read_to_string(&path).unwrap_or_default();
            contents.push_str(&render(stubs::ENV_MAIL, &values));
            write(&path, &contents)?;
        }
    }

    if auth_kit {
        for (path, contents) in auth_kit::FILES {
            // Rendered rather than copied: `tests/web.rs` names the crate, and
            // the crate is only known here. Nothing else in the kit holds a
            // `{{placeholder}}` — the views use `{{ spaced }}` for their own
            // variables, which this does not touch.
            write(&root.join(path), &render(contents, &values))?;
        }
        console::created("src/controllers/auth/ (sign in, register, reset, two-factor)");
        console::created("src/controllers/admin/ (users, roles, permissions)");
        console::created("resources/views/ (every page, Tailwind)");
        console::created("public/css/app.css, public/js/app.js");
        console::created("database/migrations/ (users, tokens, sign-in log, factors)");

        // The kit's own main.rs, registry and seeder replace the plain ones
        // the scaffold has just written.
        write(&root.join("src/main.rs"), &render(auth_kit::MAIN_RS, &values))?;
        write(&root.join("src/lib.rs"), &format!("{LIB_RS}pub mod database;\npub mod models;\npub mod support;\n"))?;
        write(&root.join("database/migrations/mod.rs"), auth_kit::MIGRATIONS_REGISTRY)?;
        write(&root.join("database/seeders/auth_kit_seeder.rs"), auth_kit::SEEDER)?;
        write(&root.join("database/seeders/mod.rs"), auth_kit::SEEDERS_REGISTRY)?;
        console::created("src/main.rs, database/seeders/auth_kit_seeder.rs");

        write(&root.join("config/auth.json"), auth_kit::CONFIG_AUTH)?;
        write(&root.join("config/rbac.json"), auth_kit::CONFIG_RBAC)?;
        write(&root.join("config/webauthn.json"), auth_kit::CONFIG_WEBAUTHN)?;
        console::created("config/auth.json, config/rbac.json, config/webauthn.json");

        for file in [".env", ".env.example"] {
            let path = root.join(file);
            let mut contents = std::fs::read_to_string(&path).unwrap_or_default();
            contents.push_str(auth_kit::ENV_ADDITIONS);
            write(&path, &contents)?;
        }
    }

    let enabled = if packages.is_empty() {
        String::new()
    } else {
        format!("\n  Packages: {}", packages.join(", "))
    };

    console::success(&format!(
        "Created {name}.{enabled}\n\n  cd {name}\n  rustlavel serve"
    ));
    Ok(())
}

/// The application's own crate root, so tests and the binary share modules.
const LIB_RS: &str = r#"pub mod controllers;
pub mod routes;
"#;

/// `database/` sits outside `src/`, so a module points at it.
const DATABASE_BRIDGE: &str = r#"//! Generated by the rustlavel CLI: bridges `database/` into the crate.

#[path = "../database/migrations/mod.rs"]
pub mod migrations;

#[path = "../database/seeders/mod.rs"]
pub mod seeders;
"#;

fn write(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

const REGISTERABLE: &[(&str, &str)] = &[
    ("telescope", "Telescope::new()"),
    ("debugbar", "DebugBar::new()"),
    ("metrics", "Metrics::new()"),
    ("otel", "OpenTelemetry::new()"),
];
/// Needs something `main.rs` does not have yet, so it gets a comment.
const NEEDS_WIRING: &[(&str, &str)] = &[
    ("rbac", "Rbac::from_config(db.clone(), app.config())?"),
    ("audit", "rustlavel::audit::Audit::new(db.clone())"),
    ("flags", "FeatureFlags::new(flags)"),
    ("vault", "Vault::from_config(app.config())?"),
    // `QueueDashboard::new` takes an `Arc<dyn Queue>`, not a database —
    // the comment used to say otherwise, and anybody pasting it got a
    // type error.
    ("queue", "QueueDashboard::new(Arc::new(DatabaseQueue::new(db.clone())) as Arc<dyn Queue>)"),
    // These three take something only the application can build: a set of
    // tools, a configured provider, an authorization server. A bare
    // `Socialite::new()` would mount two routes that answer "unknown
    // provider" to everything, which is a line that looks registered and
    // is not.
    ("mcp", "Mcp::new(server)"),
    ("oauth", "Socialite::new().provider(client)"),
    ("oauth-provider", "OAuthProvider::new(server)"),
];

/// The `.plugin(...)` lines for the packages that were asked for.
///
/// **A package that is switched on and never registered is a package that does
/// nothing**, and the generated `main.rs` used to say nothing about it: asking
/// for `--with telescope` compiled the dependency in, `/telescope` answered
/// 404, and the file gave no hint that a line was missing. The scaffold knows
/// exactly what was requested, so it writes the line.
///
/// Only the plugins that can be built from nothing are here. `Rbac` and
/// `Audit` need a database handle and `FeatureFlags` needs a store, so a line
/// for those would not compile — the auth-kit's own `main.rs` registers them,
/// because it is the one that creates the handle. A comment names them instead.
fn plugin_lines(packages: &[String], already: &[&str]) -> String {

    let mut lines = String::new();
    for (package, expression) in REGISTERABLE {
        if already.contains(package) {
            continue;
        }
        if packages.iter().any(|p| p == package) {
            lines.push_str(&format!("        .plugin({expression})\n"));
        }
    }

    let owed: Vec<&str> = NEEDS_WIRING
        .iter()
        .filter(|(package, _)| !already.contains(package))
        .filter(|(package, _)| packages.iter().any(|p| p == package))
        .map(|(_, expression)| *expression)
        .collect();
    if !owed.is_empty() {
        lines.push_str(&format!(
            "        // Also asked for, and each needing something only this\n\
             \x20       // application can build — a database handle, a store, a\n\
             \x20       // configured client. Add them here:\n\
             \x20       // {}\n",
            owed.join("\n        // ")
        ));
    }

    lines
}

/// The questions `rustlavel new` asks when nothing was asked for on the
/// command line.
///
/// **It answers itself when nobody is there.** Every prompt falls back to its
/// default if stdin is not a terminal, so a scaffold in CI produces the same
/// project it always did rather than hanging on a question. `--with`, `--all`
/// and `--yes` all skip this entirely.
///
/// Three questions, in the order somebody actually decides them: what kind of
/// application, which database, and what else to put in. Asking about
/// packages one at a time — twenty-seven yes/no questions — would be worse
/// than the flag it replaces.
fn interview() -> Vec<String> {
    if !ask::interactive() {
        return Vec::new();
    }

    let shape = ask::choose(
        "What kind of application is this?",
        &[
            (
                "A full application, with the auth starter kit",
                "Sign-in, passkeys, roles and permissions, an admin area, settings, an audit trail",
            ),
            ("An API", "Routing, JSON resources, validation and a database — no views"),
            ("A web application", "Routes, views and a database, with sign-in left to you"),
            ("Nothing but the skeleton", "One route and one controller; add packages later"),
        ],
        0,
    );

    let mut packages: Vec<String> = match shape {
        0 => vec!["auth-kit".to_string()],
        1 => ["db", "validation", "openapi"].iter().map(|p| p.to_string()).collect(),
        2 => ["db", "view", "validation"].iter().map(|p| p.to_string()).collect(),
        _ => Vec::new(),
    };

    // The database question is only worth asking when something will use one.
    // The kit needs one to boot at all, so "none" is not offered there.
    if packages.iter().any(|p| p == "db" || p == "auth-kit") {
        let needs_one = shape == 0;
        let mut engines: Vec<(&str, &str)> = vec![
            ("PostgreSQL", "The reference driver — everything is tested here first"),
            ("MySQL or MariaDB", ""),
            ("SQL Server", ""),
        ];
        if !needs_one {
            engines.push(("Decide later", "The database package is added; DATABASE_URL is left blank"));
        }

        let engine = ask::choose("Which database?", &engines, 0);
        chosen_database(engine);
    }

    let extras = ask::choose_many(
        "Anything else? (each one is a package, and what you leave out is never compiled)",
        EXTRAS,
        &[],
    );
    for index in extras {
        packages.push(EXTRAS[index].0.split_whitespace().next().unwrap_or_default().to_string());
    }

    packages.sort();
    packages.dedup();

    if packages.is_empty() {
        console::info("No packages. `rustlavel new <name> --with <package>` adds one later.");
    }
    packages
}

/// The optional packages worth offering, in the order somebody reaches for
/// them. Not every package: `auth`, `db` and the rest that a shape above
/// already implies are not choices at this point, and offering all
/// twenty-seven would make the list unreadable.
///
/// The first word of each label is the package name, which is what
/// [`interview`] reads back — so the label cannot be reworded freely, and the
/// test below checks that every one of them is still a real package.
const EXTRAS: &[(&str, &str)] = &[
    ("queue — background jobs", "Workers, scheduling, retries and a dead-letter store"),
    ("storage — files", "Local disk and S3-compatible object stores"),
    ("mail — email", "SMTP written here, with a log driver for development"),
    ("cache — caching", "Memory, file, or Redis, chosen in .env"),
    ("model-cache — a second-level cache", "Entities and query results, invalidated per table"),
    ("i18n — translations", ""),
    ("ws — WebSockets", "RFC 6455, plus channel broadcasting"),
    ("telescope — a debugging dashboard", "Requests, queries and logs, in process"),
    ("debugbar — a development overlay", "This request's queries, cache and timing, on the page"),
    ("metrics — Prometheus metrics", ""),
    ("otel — OpenTelemetry", "Traces and metrics over OTLP"),
    ("openapi — an OpenAPI document", "Generated from the router"),
    ("client — an outbound HTTP client", "TLS from rustls, with a circuit breaker"),
    ("ai — Anthropic, OpenAI and Ollama", "One API, with streaming, tools and a fake provider"),
    ("mcp — Model Context Protocol", ""),
    ("search — Elasticsearch", ""),
    ("ldap — a directory", "Bind, search, and authenticating against it"),
    ("oauth — social login", "The client half: sign in with Google, GitHub and the rest"),
    ("oauth-provider — be a provider", "Authorization code with PKCE, refresh rotation, revocation"),
    ("vault — secrets", "OpenBao or HashiCorp Vault, with dynamic database credentials"),
    ("flags — feature flags", "Runtime switches, per user or per tenant"),
];

/// Remember the engine somebody picked, so `.env` can be written with a URL
/// they can actually fill in rather than a PostgreSQL one they have to correct.
fn chosen_database(index: usize) {
    DATABASE.with(|cell| cell.set(index));
}

thread_local! {
    static DATABASE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The `DATABASE_URL` line for the engine that was chosen.
fn database_url_line() -> &'static str {
    match DATABASE.with(std::cell::Cell::get) {
        1 => "DATABASE_URL=mysql://root:secret@127.0.0.1:3306/app",
        2 => "DATABASE_URL=sqlserver://sa:secret@127.0.0.1:1433/app",
        3 => "DATABASE_URL=",
        _ => "DATABASE_URL=postgres://postgres:secret@127.0.0.1:5432/app",
    }
}

#[cfg(test)]
mod tests {
    /// **Every package that ships a plugin has to be accounted for — in the
    /// file that is actually written.**
    ///
    /// The first version of this test called `plugin_lines` and passed while
    /// the feature was broken for every `auth-kit` project: `auth-kit` writes
    /// its own `main.rs`, that template had no `{{plugins}}` in it, and it is
    /// written second so it wins. The helper was right and the artifact was
    /// wrong, which is the only kind of bug a test on the helper cannot see.
    ///
    /// So this renders both templates and reads the output.
    #[test]
    fn every_package_that_ships_a_plugin_is_named_in_the_main_rs_that_gets_written() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/ is the parent of this crate");

        for (package, _) in PACKAGES {
            if *package == "auth-kit" {
                continue;
            }
            if !ships_a_plugin(&crates.join(format!("rustlavel-{package}")).join("src")) {
                continue;
            }

            // Both shapes: the plain scaffold, and the starter kit — which
            // already registers `rbac` and `audit` in its own template.
            for (template, already) in [
                (stubs::MAIN_RS, &[] as &[&str]),
                (stubs::MAIN_RS_DB, &[]),
                (crate::auth_kit::MAIN_RS, &["rbac", "audit"][..]),
            ] {
                let mut values = BTreeMap::new();
                values.insert("plugins", plugin_lines(&[(*package).to_string()], already));
                values.insert("crate_name", "app".to_string());
                let rendered = stubs::render(template, &values);

                let mentioned = rendered.contains(package)
                    || REGISTERABLE
                        .iter()
                        .chain(NEEDS_WIRING)
                        .find(|(name, _)| name == package)
                        .is_some_and(|(_, expression)| {
                            rendered.contains(expression.split("::").next().unwrap_or(expression))
                        });

                assert!(
                    mentioned || already.contains(package),
                    "`--with {package}` produces a main.rs that says nothing about it:\n{rendered}"
                );
            }
        }
    }

    /// **Every table the kit's migrations create has to be in the backup.**
    ///
    /// Three were added in one afternoon and none of them reached
    /// `OWN_TABLES`, so the Backup screen produced a file that silently
    /// omitted the navigation menus, the whole audit trail and the password
    /// history — and said nothing, because a hand-kept list cannot know what
    /// it is missing. This reads the migrations instead.
    #[test]
    fn the_backup_covers_every_table_the_kit_creates() {
        let migrations = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("templates/auth-kit/database/migrations");
        let backup = include_str!("../templates/auth-kit/src/support/backup.rs");

        let mut missing = Vec::new();
        for entry in std::fs::read_dir(&migrations).expect("the migrations directory") {
            let path = entry.expect("a readable entry").path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("a readable migration");

            // `schema.create("name", …)` — the tables this file makes.
            for (at, _) in source.match_indices(".create(\"") {
                let rest = &source[at + ".create(\"".len()..];
                let Some(end) = rest.find('"') else { continue };
                let table = &rest[..end];

                // `backups` is deliberately absent — a dump that recorded the
                // row describing itself would restore a catalogue of files
                // that are no longer on disk.
                if table == "backups" {
                    continue;
                }
                if !backup.contains(&format!("\"{table}\"")) {
                    missing.push(format!("{table} (from {})", path.file_name().unwrap().to_string_lossy()));
                }
            }
        }

        // The kit's registry also extends with another crate's migrations, and
        // `audit_logs` came from there — which is exactly why the first
        // version of this test passed while that table was missing. A check
        // that reads one directory is the same hand-kept list in a different
        // shape.
        //
        // Only crates whose table names are *fixed* are checked here.
        // `rustlavel-rbac` lets an application rename its five, so `tables()`
        // asks the store for them at runtime rather than hard-coding them, and
        // a static check would be looking for the wrong names.
        let registry = include_str!("auth_kit.rs");
        for package in ["audit"] {
            if !registry.contains(&format!("rustlavel::{package}::migrations()")) {
                continue;
            }
            let source = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .expect("crates/")
                    .join(format!("rustlavel-{package}/src/tables.rs")),
            )
            .unwrap_or_default();

            let marker = "pub const TABLE: &str = \"";
            for (at, _) in source.match_indices(marker) {
                let rest = &source[at + marker.len()..];
                let Some(end) = rest.find('"') else { continue };
                let table = &rest[..end];

                // Either form counts. Naming the crate's own constant is the
                // better one — a rename there cannot then drop the table out
                // of every backup — so a check that insisted on the literal
                // would be pushing towards the worse spelling.
                let by_literal = backup.contains(&format!("\"{table}\""));
                let by_constant = backup.contains(&format!("{package}::TABLE"));
                if !by_literal && !by_constant {
                    missing.push(format!("{table} (from rustlavel-{package})"));
                }
            }
        }

        assert!(
            missing.is_empty(),
            "these tables are created by the kit and would not be in a backup: {missing:#?}"
        );
    }

    /// **Every `MAIL_*` variable has to reach the mailer, not just the page.**
    ///
    /// `Config` knows only what a file under `config/` declares — there is no
    /// automatic `MAIL_HOST` to `mail.host` mapping — while the starter kit's
    /// settings screen reads the environment directly. So a variable with a
    /// line in `.env` and no path in `config/mail.json` produces a screen that
    /// says the environment is in charge of a value the mailer never sees.
    /// That was true of five of them.
    #[test]
    fn every_mail_variable_in_the_env_is_wired_through_config() {
        let catalogue = include_str!("../templates/auth-kit/src/support/settings.rs");
        let declared: Vec<&str> = catalogue
            .match_indices("MAIL_")
            .map(|(at, _)| {
                let rest = &catalogue[at..];
                let end = rest
                    .find(|c: char| !c.is_ascii_uppercase() && c != '_')
                    .unwrap_or(rest.len());
                &rest[..end]
            })
            .filter(|name| *name != "MAIL_DRIVERS")
            .collect();

        assert!(declared.len() >= 5, "the catalogue no longer names any: {declared:?}");

        for variable in declared {
            assert!(
                stubs::ENV_MAIL.contains(&format!("{variable}=")),
                "the settings screen reads {variable} and `.env` has no line for it"
            );
            assert!(
                stubs::CONFIG_MAIL.contains(&format!("${{{variable}")),
                "{variable} is in `.env` but config/mail.json does not read it, so it reaches \
                 the settings screen and never the mailer"
            );
        }
    }

    /// Whether any file under this directory implements `Plugin`.
    fn ships_a_plugin(directory: &std::path::Path) -> bool {
        let Ok(entries) = std::fs::read_dir(directory) else { return false };
        entries.filter_map(Result::ok).any(|entry| {
            let path = entry.path();
            if path.is_dir() {
                return ships_a_plugin(&path);
            }
            path.extension().is_some_and(|e| e == "rs")
                && std::fs::read_to_string(&path)
                    .is_ok_and(|text| text.contains("impl Plugin for"))
        })
    }

    /// A package that is switched on and never registered does nothing, and
    /// the generated file used to say nothing about it.
    #[test]
    fn the_scaffold_registers_the_plugins_it_was_asked_for() {
        let asked = ["telescope", "debugbar", "metrics"].map(str::to_string);
        let lines = plugin_lines(&asked, &[]);

        assert!(lines.contains(".plugin(Telescope::new())"), "{lines}");
        assert!(lines.contains(".plugin(DebugBar::new())"), "{lines}");
        assert!(lines.contains(".plugin(Metrics::new())"), "{lines}");

        // Nothing asked for, nothing written — a bare `main.rs` stays bare.
        assert_eq!(plugin_lines(&[], &[]), "");
    }

    /// The ones that need a database handle cannot be registered from a file
    /// that has none, so they are named in a comment rather than emitted as a
    /// line that would not compile.
    #[test]
    fn a_plugin_that_needs_wiring_is_named_rather_than_guessed_at() {
        let lines = plugin_lines(&["rbac".to_string(), "audit".to_string()], &[]);

        assert!(!lines.contains(".plugin(Rbac"), "an uncompilable line was written: {lines}");
        assert!(lines.contains("// Rbac::from_config"), "{lines}");
        assert!(lines.contains("// rustlavel::audit::Audit::new"), "{lines}");
    }

    /// Every expression the generator can write has to name a type the prelude
    /// exports, because the generated `main.rs` imports the prelude and
    /// nothing else. `Metrics` was not in it, and the line did not compile.
    #[test]
    fn every_registered_plugin_is_reachable_from_the_prelude() {
        let prelude = include_str!("../../rustlavel/src/lib.rs");
        let preludes_at = prelude.find("pub mod prelude").expect("the prelude moved");
        let exports = &prelude[preludes_at..];

        for kind in ["Telescope", "DebugBar", "Metrics"] {
            assert!(
                exports.contains(&format!("pub use crate::{kind};")),
                "{kind} is written into main.rs but the prelude does not export it"
            );
        }
    }

    use super::*;

    /// The list `--with` accepts has to be the list the meta-crate offers.
    ///
    /// These two drifted once already: `rbac` and `flags` were features of
    /// `rustlavel` for a release each before `rustlavel new --with rbac` would
    /// take them, which is a confusing way to find out a package exists. The
    /// manifest is the source of truth, so this reads it rather than repeating
    /// the list a third time.
    #[test]
    fn every_feature_on_the_meta_crate_is_a_package_the_scaffold_accepts() {
        let manifest = include_str!("../../rustlavel/Cargo.toml");

        let features: Vec<&str> = manifest
            .lines()
            .skip_while(|line| line.trim() != "[features]")
            .skip(1)
            .take_while(|line| !line.trim_start().starts_with('['))
            .filter_map(|line| line.split_once('=').map(|(name, _)| name.trim()))
            // `default` and `full` are not packages, they are collections of
            // them; every other key is a package the scaffold should know.
            .filter(|name| !name.is_empty() && *name != "default" && *name != "full")
            .collect();

        assert!(features.len() > 20, "the feature list was not found: {features:?}");

        let known: Vec<&str> = PACKAGES.iter().map(|(name, _)| *name).collect();
        let missing: Vec<&&str> = features.iter().filter(|f| !known.contains(f)).collect();
        assert!(
            missing.is_empty(),
            "these are features of the rustlavel crate but `--with` refuses them: {missing:?}"
        );
    }

    /// And the other direction, except for the one that is not a feature.
    #[test]
    fn the_scaffold_offers_nothing_the_meta_crate_cannot_turn_on() {
        let manifest = include_str!("../../rustlavel/Cargo.toml");
        for (package, _) in PACKAGES {
            // `auth-kit` is scaffolding rather than a feature flag: it writes
            // files and expands into the packages that code imports.
            if *package == "auth-kit" {
                continue;
            }
            assert!(
                manifest.contains(&format!("\n{package} = [")),
                "`--with {package}` is offered, but `rustlavel` has no such feature"
            );
        }
    }

    #[test]
    fn the_package_list_is_sorted_so_the_error_message_reads_in_order() {
        let names: Vec<&str> = PACKAGES.iter().map(|(name, _)| *name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "keep PACKAGES alphabetical");
    }
}
