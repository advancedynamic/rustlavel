//! `rustlavel new <app>` — scaffold an application.
//!
//! Deliberately slim, following Laravel 11+: routes, one controller, config,
//! public, tests. Everything else arrives when a package is added.

use crate::auth_kit;
use crate::naming;
use crate::stubs::{self, render};
use crate::console;
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
            }
            "--all" => packages = PACKAGES.iter().map(|(name, _)| (*name).to_string()).collect(),
            other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
            other => name = Some(other.to_string()),
        }
    }

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

    let name = name.ok_or("usage: rustlavel new <name> [--with db,view] [--local <path>]")?;
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
    values.insert("plugins", plugin_lines(&packages));
    values.insert("name", crate_name.clone());
    values.insert("crate_name", crate_name.clone());
    values.insert("app_name", naming::pascal(&name));
    values.insert("dependency", dependency);

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
fn plugin_lines(packages: &[String]) -> String {
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
        ("queue", "QueueDashboard::new(db.clone())"),
        // These three take something only the application can build: a set of
        // tools, a configured provider, an authorization server. A bare
        // `Socialite::new()` would mount two routes that answer "unknown
        // provider" to everything, which is a line that looks registered and
        // is not.
        ("mcp", "Mcp::new(server)"),
        ("oauth", "Socialite::new().provider(client)"),
        ("oauth-provider", "OAuthProvider::new(server)"),
    ];

    let mut lines = String::new();
    for (package, expression) in REGISTERABLE {
        if packages.iter().any(|p| p == package) {
            lines.push_str(&format!("        .plugin({expression})\n"));
        }
    }

    let owed: Vec<&str> = NEEDS_WIRING
        .iter()
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

#[cfg(test)]
mod tests {
    /// **Every package that ships a plugin has to be accounted for.**
    ///
    /// The lists above are maintained by hand, and the first version of them
    /// covered eight of the twelve: `otel` ships a no-argument plugin and was
    /// silently left out, and `mcp`, `oauth` and `oauth-provider` were not
    /// even named. This test counts rather than trusts — it reads the crates
    /// directory, finds every `impl Plugin for`, and fails on any package the
    /// scaffold offers but says nothing about.
    #[test]
    fn every_package_that_ships_a_plugin_is_registered_or_named() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/ is the parent of this crate");

        let mut unaccounted = Vec::new();
        for (package, _) in PACKAGES {
            // `auth-kit` is scaffolding, not a crate.
            if *package == "auth-kit" {
                continue;
            }
            let source = crates.join(format!("rustlavel-{package}")).join("src");
            if !ships_a_plugin(&source) {
                continue;
            }
            let named = plugin_lines(&[(*package).to_string()]);
            if named.is_empty() {
                unaccounted.push(*package);
            }
        }

        assert!(
            unaccounted.is_empty(),
            "these packages ship a plugin and the scaffold neither registers nor names them: \
             {unaccounted:?}"
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
        let lines = plugin_lines(&asked);

        assert!(lines.contains(".plugin(Telescope::new())"), "{lines}");
        assert!(lines.contains(".plugin(DebugBar::new())"), "{lines}");
        assert!(lines.contains(".plugin(Metrics::new())"), "{lines}");

        // Nothing asked for, nothing written — a bare `main.rs` stays bare.
        assert_eq!(plugin_lines(&[]), "");
    }

    /// The ones that need a database handle cannot be registered from a file
    /// that has none, so they are named in a comment rather than emitted as a
    /// line that would not compile.
    #[test]
    fn a_plugin_that_needs_wiring_is_named_rather_than_guessed_at() {
        let lines = plugin_lines(&["rbac".to_string(), "audit".to_string()]);

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
