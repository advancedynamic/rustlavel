//! `rustlavel new <app>` — scaffold an application.
//!
//! Deliberately slim, following Laravel 11+: routes, one controller, config,
//! public, tests. Everything else arrives when a package is added.

use crate::naming;
use crate::stubs::{self, render};
use crate::console;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn run(args: &[String]) -> Result<(), String> {
    let mut name = None;
    let mut local_framework: Option<String> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // Point the new app at a framework checkout instead of crates.io,
            // which is what the framework's own smoke tests need.
            "--local" => {
                local_framework =
                    Some(iter.next().ok_or("--local needs a path to the framework workspace")?.clone())
            }
            other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
            other => name = Some(other.to_string()),
        }
    }

    let name = name.ok_or("usage: rustlavel new <name> [--local <framework path>]")?;
    let crate_name = naming::snake(&name);
    let root = PathBuf::from(&name);

    if root.exists() {
        return Err(format!("`{name}` already exists"));
    }

    let dependency = match &local_framework {
        Some(path) => {
            let absolute = std::fs::canonicalize(path)
                .map_err(|e| format!("cannot resolve --local path `{path}`: {e}"))?;
            format!("path = \"{}/crates/rustlavel\"", absolute.display())
        }
        None => format!("version = \"{}\"", env!("CARGO_PKG_VERSION")),
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
        ("config/app.json", stubs::CONFIG_APP),
        ("public/README.md", stubs::PUBLIC_KEEP),
        ("src/main.rs", stubs::MAIN_RS),
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
    write(&root.join("src/lib.rs"), LIB_RS)?;
    console::created("src/lib.rs");

    console::success(&format!(
        "Created {name}.\n\n  cd {name}\n  rustlavel serve"
    ));
    Ok(())
}

/// The application's own crate root, so tests and the binary share modules.
const LIB_RS: &str = r#"pub mod controllers;
pub mod routes;
"#;

fn write(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))
}
