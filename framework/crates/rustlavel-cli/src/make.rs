//! The `make:*` generators.

use crate::naming;
use crate::project::{self, Project};
use crate::stubs::{self, render};
use crate::console;
use std::collections::BTreeMap;
use std::path::Path;

pub fn run(command: &str, args: &[String]) -> Result<(), String> {
    let name = args
        .first()
        .ok_or_else(|| format!("usage: rustlavel {command} <name>"))?;
    let project = Project::discover()?;

    match command {
        "make:controller" => controller(&project, name),
        "make:middleware" => middleware(&project, name),
        other => Err(format!("unknown generator `{other}`")),
    }
}

fn controller(project: &Project, name: &str) -> Result<(), String> {
    // `make:controller Post` and `make:controller PostController` both mean the
    // same thing, so the suffix is normalised rather than doubled.
    let base = name.trim_end_matches("Controller").trim_end_matches("_controller");
    let class = format!("{}Controller", naming::pascal(base));
    let module = naming::snake(&class);

    let mut values = BTreeMap::new();
    values.insert("class", class.clone());

    let path = project.root.join("src/controllers").join(format!("{module}.rs"));
    write_new(&path, &render(stubs::CONTROLLER_STUB, &values))?;
    console::created(&relative(project, &path));

    let mod_file = project.root.join("src/controllers/mod.rs");
    if project::declare_module(&mod_file, &module)? {
        console::updated(&relative(project, &mod_file));
    }

    console::success(&format!(
        "{class} created. Register it:\n\n  r.get(\"/{}\", {class}::index);",
        naming::kebab(&naming::plural(base))
    ));
    Ok(())
}

fn middleware(project: &Project, name: &str) -> Result<(), String> {
    let function = naming::snake(name);

    let mut values = BTreeMap::new();
    values.insert("function", function.clone());

    let path = project.root.join("src/middleware").join(format!("{function}.rs"));
    write_new(&path, &render(stubs::MIDDLEWARE_STUB, &values))?;
    console::created(&relative(project, &path));

    let mod_file = project.root.join("src/middleware/mod.rs");
    if project::declare_module(&mod_file, &function)? {
        console::updated(&relative(project, &mod_file));
    }

    // The module tree only exists once something declares it.
    let lib = project.root.join("src/lib.rs");
    if let Ok(contents) = std::fs::read_to_string(&lib)
        && !contents.contains("pub mod middleware;") {
            std::fs::write(&lib, format!("{contents}pub mod middleware;\n")).map_err(|e| e.to_string())?;
            console::updated(&relative(project, &lib));
        }

    console::success(&format!(
        "{function} created. Apply it:\n\n  r.middleware(middleware::{function}::{function});"
    ));
    Ok(())
}

/// Write a file, refusing to clobber one that already exists.
fn write_new(path: &Path, contents: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn relative(project: &Project, path: &Path) -> String {
    path.strip_prefix(&project.root).unwrap_or(path).display().to_string()
}
