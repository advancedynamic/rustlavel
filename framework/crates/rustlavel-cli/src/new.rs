//! `rustlavel new <app>` — scaffold an application.
//!
//! Deliberately slim, following Laravel 11+: routes, one controller, config,
//! public, tests. Everything else arrives when a package is added.

use crate::naming;
use crate::stubs::{self, render};
use crate::console;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The packages `--with` can turn on, and what each one needs on disk.
const PACKAGES: &[(&str, &[&str])] = &[
    ("ai", &[]),
    ("auth", &["storage/sessions"]),
    ("cache", &["storage/cache"]),
    ("client", &[]),
    ("db", &["database/migrations", "database/seeders"]),
    ("debugbar", &[]),
    ("i18n", &["lang"]),
    ("ldap", &[]),
    ("mail", &["resources/views"]),
    ("mcp", &[]),
    ("metrics", &[]),
    ("oauth", &["storage/sessions"]),
    ("oauth-provider", &["storage/sessions"]),
    ("openapi", &[]),
    ("otel", &[]),
    ("queue", &["database/migrations"]),
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
