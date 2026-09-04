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
        // The font, byte for byte: not through `render`, which would rewrite
        // whatever inside a woff2 happened to look like a placeholder.
        for (path, bytes) in auth_kit::BINARY_FILES {
            write_bytes(&root.join(path), bytes)?;
        }
        console::created("src/controllers/auth/ (sign in, register, reset, two-factor)");
        console::created("src/controllers/admin/ (users, roles, permissions)");
        console::created("resources/views/ (every page, Tailwind)");
        console::created("public/css/app.css, public/js/app.js");
        console::created("public/fonts/ (Inter, self-hosted, OFL)");
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

/// The same, for a file that is not text.
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

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

    /// Every file under `directory` whose name ends in one of `suffixes`.
    fn gather(directory: &std::path::Path, suffixes: &[&str], into: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(directory) else { return };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                gather(&path, suffixes, into);
            } else if suffixes.iter().any(|s| path.to_string_lossy().ends_with(s))
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                into.push((path.to_string_lossy().into_owned(), text));
            }
        }
    }

    /// A setting nobody reads is a switch that does nothing.
    ///
    /// This is the bug that keeps coming back, and it never shows up in a
    /// review: the catalogue declares the key, the tab renders a control for
    /// it, saving writes a row — and no code anywhere asks what the row says.
    /// Six mail settings, a timezone, a date format, a backup destination and
    /// a first-day-of-week all shipped that way. So enumerate the catalogue
    /// rather than trust it, and require a reader outside the settings screen
    /// itself for every key in it.
    #[test]
    fn every_setting_in_the_catalogue_is_read_by_something() {
        let kit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/auth-kit");
        let catalogue = std::fs::read_to_string(kit.join("src/support/settings.rs")).unwrap();
        let body = catalogue
            .split_once("pub const CATALOGUE")
            .and_then(|(_, rest)| rest.split_once("\n];"))
            .expect("the catalogue is no longer a `pub const CATALOGUE` ending in `];`")
            .0;

        let mut keys: Vec<&str> = Vec::new();
        for piece in body.split('"').skip(1).step_by(2) {
            let looks_like_a_key = piece.contains('.')
                && !piece.is_empty()
                && piece
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_')
                // Every segment starts with a letter, which is what keeps a
                // default like `127.0.0.1` from reading as a key.
                && piece.split('.').all(|part| part.starts_with(|c: char| c.is_ascii_lowercase()));
            if looks_like_a_key && !keys.contains(&piece) {
                keys.push(piece);
            }
        }
        assert!(keys.len() > 40, "only {} keys parsed out of the catalogue", keys.len());

        // Everything except the settings screen, which renders a control for a
        // key whether or not anything honours it — the very thing being tested.
        let mut files = Vec::new();
        gather(&kit.join("src"), &[".rs"], &mut files);
        gather(&kit.join("resources"), &[".html"], &mut files);
        files.retain(|(path, _)| {
            !path.ends_with("support/settings.rs")
                && !path.contains("settings_controller")
                && !path.contains("views/settings")
        });

        let dead: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|key| {
                let quoted = format!("\"{key}\"");
                !files.iter().any(|(_, text)| text.contains(&quoted))
            })
            .collect();

        assert!(
            dead.is_empty(),
            "these settings are declared, rendered on a tab and saved to the database, and \
             nothing outside the settings screen ever reads them — wire each one up or take \
             it off the tab: {dead:?}"
        );
    }

    /// A permission the seeder creates and nothing checks, or the other way
    /// round, is the same bug as a setting nobody reads.
    ///
    /// One half locks a page nobody can ever reach — the guard names a
    /// permission that is in no role, so every request is refused. The other
    /// half is a row in the permissions table that grants nothing, which an
    /// administrator will tick and then wonder about.
    #[test]
    fn every_permission_is_both_seeded_and_checked() {
        let kit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/auth-kit");
        // The list lives in the seeder the CLI writes, which is a string
        // here and Rust only once a project exists — so read it out of the
        // string rather than referring to a constant that is not ours.
        let seeder = crate::auth_kit::SEEDER;
        let table = seeder
            .split_once("const PERMISSIONS: &[(&str, &str)] = &[")
            .and_then(|(_, rest)| rest.split_once("\n];"))
            .expect("the seeder no longer declares PERMISSIONS as a table")
            .0;
        let seeded: Vec<&str> = table
            .split("(\"")
            .skip(1)
            .filter_map(|piece| piece.split_once('"').map(|(name, _)| name))
            .collect();
        assert!(seeded.len() > 15, "only {} permissions parsed: {seeded:?}", seeded.len());

        let mut files = Vec::new();
        gather(&kit.join("src"), &[".rs"], &mut files);
        gather(&kit.join("resources"), &[".html"], &mut files);

        for name in &seeded {
            let quoted = format!("\"{name}\"");
            assert!(
                files.iter().any(|(_, text)| text.contains(&quoted)),
                "the seeder creates `{name}` and nothing in the generated application asks for \
                 it, so granting it does nothing"
            );
        }

        // And the other direction: anything a guard or a `can` names has to be
        // a permission the seeder actually creates.
        for (path, text) in &files {
            for prefix in ["guard(\"", "can(\"", "@can(\""] {
                let mut rest = text.as_str();
                while let Some(at) = rest.find(prefix) {
                    rest = &rest[at + prefix.len()..];
                    let Some(end) = rest.find('"') else { break };
                    let name = &rest[..end];
                    // Only the `noun.verb` shape is a permission; `can("...")`
                    // also appears in prose and in other kinds of argument.
                    if name.split('.').count() == 2
                        && name.chars().all(|c| c.is_ascii_lowercase() || c == '.' || c == '_')
                    {
                        assert!(
                            seeded.contains(&name),
                            "{path} checks for `{name}`, which the seeder never creates — the \
                             page it guards can never be opened by anybody"
                        );
                    }
                }
            }
        }
    }

    /// The stylesheet the kit ships has to cover the markup the kit ships.
    ///
    /// `public/css/app.css` is a Tailwind build committed so a project needs no
    /// Node toolchain, and that is the trap: adding a class to a template is
    /// free, and the stylesheet only learns about it when somebody remembers to
    /// rebuild. It went stale exactly that way — a release added a search box,
    /// a notification list, toasts, pagination and tab pills, and shipped all
    /// of them with 58 of their classes undefined, so the markup was there and
    /// the styling simply was not.
    ///
    /// Rebuild with:
    ///
    /// ```text
    /// npx @tailwindcss/cli -i resources/css/app.css -o public/css/app.css --minify
    /// ```
    #[test]
    fn the_committed_stylesheet_covers_the_classes_the_templates_use() {
        let kit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/auth-kit");
        let css = std::fs::read_to_string(kit.join("public/css/app.css")).unwrap();

        // The named component classes: every `@utility` in the source has to
        // have survived into the build, and those are the ones a template
        // spells out by name rather than composing from utilities.
        let source = std::fs::read_to_string(kit.join("resources/css/app.css")).unwrap();
        let named: Vec<&str> = source
            .match_indices("@utility ")
            .map(|(at, _)| {
                let rest = &source[at + "@utility ".len()..];
                let end = rest
                    .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                    .unwrap_or(rest.len());
                &rest[..end]
            })
            .collect();
        assert!(named.len() > 20, "only {} utilities parsed: {named:?}", named.len());

        let mut used = Vec::new();
        gather(&kit.join("resources/views"), &[".html"], &mut used);
        gather(&kit.join("src"), &[".rs"], &mut used);
        gather(&kit.join("public/js"), &[".js"], &mut used);

        let mut undefined: Vec<String> = Vec::new();
        for name in named {
            // Only complain about a class something actually writes: a utility
            // defined and never used is dead weight, not a broken page, and
            // Tailwind sweeps it out of the build on purpose.
            let quoted = [format!("\"{name}"), format!(" {name} "), format!(" {name}\"")];
            let is_used = used
                .iter()
                .any(|(_, text)| quoted.iter().any(|form| text.contains(form.as_str())));
            if is_used && !css.contains(&format!(".{name}")) {
                undefined.push(name.to_string());
            }
        }

        assert!(
            undefined.is_empty(),
            "these classes are written into the generated markup and are not in the stylesheet \
             that ships beside it, so the elements wearing them render unstyled — rebuild \
             `public/css/app.css` from `resources/css/app.css`: {undefined:?}"
        );
    }

    /// A button labelled "(Default)" has to restore the default.
    ///
    /// Settings → Appearance offers quick presets, one of them named for the
    /// value the catalogue declares. Change that value and the button keeps
    /// handing back the old one — a control that silently disagrees with the
    /// thing it names, which is how three of them ended up restoring the
    /// colours of a design two versions old.
    #[test]
    fn the_default_appearance_presets_are_the_declared_defaults() {
        let kit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/auth-kit");
        let catalogue = std::fs::read_to_string(kit.join("src/support/settings.rs")).unwrap();
        let tab = std::fs::read_to_string(
            kit.join("resources/views/settings/tabs/appearance.rl.html"),
        )
        .unwrap();

        // The declared default for one `s("key", Kind::Colour, "#value")`.
        let declared = |key: &str| -> String {
            let at = catalogue
                .find(&format!("s(\"{key}\", Kind::Colour, \""))
                .unwrap_or_else(|| panic!("`{key}` is no longer a colour in the catalogue"));
            let rest = &catalogue[at..];
            let start = rest.rfind('"').map(|_| rest.find(", \"").unwrap() + 3).unwrap();
            let end = rest[start..].find('"').unwrap() + start;
            rest[start..end].to_string()
        };

        // Each preset in the order the controller writes its keys.
        for keys in [
            vec!["theme.brand"],
            vec![
                "theme.login.light.from",
                "theme.login.light.to",
                "theme.login.dark.from",
                "theme.login.dark.to",
            ],
            vec![
                "theme.sidebar.light.bg",
                "theme.sidebar.light.text",
                "theme.sidebar.light.active_bg",
                "theme.sidebar.light.active_text",
                "theme.sidebar.dark.bg",
                "theme.sidebar.dark.text",
                "theme.sidebar.dark.active_bg",
                "theme.sidebar.dark.active_text",
            ],
        ] {
            let wanted: Vec<String> = keys.iter().map(|k| declared(k)).collect();
            let preset = format!("data-preset=\"{}\"", wanted.join(","));
            assert!(
                tab.contains(&preset),
                "the Appearance tab has no preset restoring the declared defaults for {keys:?}. \
                 Its \"(Default)\" button hands back something else, so pressing it moves the \
                 application away from the state it names. Expected {preset}"
            );
        }
    }

    /// The font has to arrive as a font.
    ///
    /// Everything in `FILES` goes through the placeholder renderer on the way
    /// out, which is right for a template and fatal for a woff2: any two
    /// braces that happened to line up inside the compressed stream would be
    /// rewritten, and the file would arrive the right size and unreadable.
    /// `BINARY_FILES` exists to keep them apart, and this is what keeps a font
    /// from being added to the wrong list.
    #[test]
    fn the_binary_files_are_carried_byte_for_byte() {
        let kit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/auth-kit");

        for (path, bytes) in crate::auth_kit::BINARY_FILES {
            let on_disk = std::fs::read(kit.join(path))
                .unwrap_or_else(|e| panic!("{path} is in BINARY_FILES and not on disk: {e}"));
            assert_eq!(
                *bytes, &on_disk[..],
                "{path} does not match the file it is embedded from"
            );
        }

        // A woff2 begins `wOF2`. If one ever ends up in `FILES` instead, the
        // renderer will have had a chance at it.
        for (path, contents) in crate::auth_kit::FILES {
            assert!(
                !path.ends_with(".woff2") && !path.ends_with(".woff") && !path.ends_with(".png"),
                "{path} is binary and is in FILES, where every entry is run through the \
                 placeholder renderer — move it to BINARY_FILES"
            );
            let _ = contents;
        }

        let fonts: Vec<&str> = crate::auth_kit::BINARY_FILES
            .iter()
            .map(|(path, _)| *path)
            .filter(|path| path.ends_with(".woff2"))
            .collect();
        assert!(!fonts.is_empty(), "the kit no longer ships a font of its own");

        // Self-hosted means the stylesheet asks this application for it.
        let css = std::fs::read_to_string(kit.join("resources/css/app.css")).unwrap();
        for path in fonts {
            let url = path.trim_start_matches("public");
            assert!(
                css.contains(&format!("url(\"{url}\")")),
                "{path} ships with the kit and no @font-face asks for it"
            );
        }
        // The comments in that file name the CDNs they explain avoiding, so
        // ask the CSS rather than the prose around it.
        let mut rules = String::new();
        let mut rest = css.as_str();
        while let Some(at) = rest.find("/*") {
            rules.push_str(&rest[..at]);
            rest = match rest[at..].find("*/") {
                Some(end) => &rest[at + end + 2..],
                None => "",
            };
        }
        rules.push_str(rest);
        assert!(
            !rules.contains("fonts.googleapis.com") && !rules.contains("fonts.gstatic.com"),
            "the stylesheet reaches out to a font CDN, which is what self-hosting was for"
        );
    }

    /// A template file that is not in a manifest never reaches a project.
    ///
    /// The kit's files live on disk so they can be read and edited, and reach
    /// an application only through `FILES` or `BINARY_FILES`. Adding one and
    /// forgetting the manifest produces a scaffold that is missing it — with
    /// no error, because nothing looked for it. Listing a file that has since
    /// been deleted fails the build instead, which is the harmless direction.
    #[test]
    fn every_template_file_is_in_a_manifest() {
        let kit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates/auth-kit");
        let listed: Vec<&str> = crate::auth_kit::FILES
            .iter()
            .map(|(path, _)| *path)
            .chain(crate::auth_kit::BINARY_FILES.iter().map(|(path, _)| *path))
            .collect();

        let mut on_disk = Vec::new();
        for directory in ["src", "resources", "public", "config", "database", "tests"] {
            let mut found = Vec::new();
            gather(&kit.join(directory), &[""], &mut found);
            for (path, _) in found {
                let relative = path
                    .strip_prefix(&format!("{}/", kit.display()))
                    .unwrap_or(&path)
                    .to_string();
                on_disk.push(relative);
            }
        }
        assert!(on_disk.len() > 80, "only {} template files found", on_disk.len());

        let missing: Vec<&String> = on_disk
            .iter()
            .filter(|path| !listed.contains(&path.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "these files are in `templates/auth-kit/` and in neither manifest, so `rustlavel \
             new` writes a project without them: {missing:?}"
        );
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
