//! Locating and reading the application the command is being run inside.

use std::path::{Path, PathBuf};

pub struct Project {
    pub root: PathBuf,
    pub crate_name: String,
}

impl Project {
    /// Walk up from the current directory looking for the application that
    /// depends on rustlavel, so commands work from any subdirectory.
    pub fn discover() -> Result<Project, String> {
        let start = std::env::current_dir().map_err(|e| e.to_string())?;

        for directory in start.ancestors() {
            let manifest = directory.join("Cargo.toml");
            let Ok(contents) = std::fs::read_to_string(&manifest) else { continue };
            if !contents.contains("rustlavel") {
                continue;
            }
            let crate_name = package_name(&contents)
                .ok_or_else(|| format!("{} has no [package] name", manifest.display()))?;
            return Ok(Project { root: directory.to_path_buf(), crate_name });
        }

        Err("not inside a rustlavel application (no Cargo.toml depending on rustlavel found)".into())
    }

    /// Directories worth watching for a reload.
    pub fn watched(&self) -> Vec<PathBuf> {
        ["src", "config", "routes", "resources", ".env"]
            .iter()
            .map(|entry| self.root.join(entry))
            .filter(|path| path.exists())
            .collect()
    }
}

fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package
            && let Some(value) = line.strip_prefix("name") {
                return value
                    .trim_start_matches([' ', '='])
                    .trim()
                    .trim_matches('"')
                    .to_string()
                    .into();
            }
    }
    None
}

/// Append `pub mod <name>;` to a module file, keeping the list sorted and not
/// duplicating an entry that is already there.
pub fn declare_module(mod_file: &Path, module: &str) -> Result<bool, String> {
    let declaration = format!("pub mod {module};");
    let existing = std::fs::read_to_string(mod_file).unwrap_or_default();

    if existing.lines().any(|line| line.trim() == declaration) {
        return Ok(false);
    }

    // **Insert, do not rewrite.** This used to collect every line, drop the
    // blank ones and sort the lot — which is correct for a file that is
    // nothing but declarations, and destroys one that also holds a trait, a
    // function or a comment. `src/modules/mod.rs` is such a file, and the
    // first `make:module` turned it into sorted fragments.
    //
    // So the declaration goes in among the other declarations, in order, and
    // every other line stays exactly where it was.
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();

    let declarations: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("pub mod "))
        .map(|(at, _)| at)
        .collect();

    let at = match declarations.first() {
        // After the last declaration that sorts before this one, so the list
        // stays alphabetical without anything else moving.
        Some(_) => declarations
            .iter()
            .rev()
            .find(|at| lines[**at].trim() < declaration.as_str())
            .map(|at| at + 1)
            .unwrap_or(declarations[0]),
        // No declarations yet: above everything, where the next reader looks.
        None => 0,
    };
    lines.insert(at, declaration);

    if let Some(parent) = mod_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(mod_file, format!("{}\n", lines.join("\n"))).map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_package_name() {
        let manifest = "[package]\nname = \"blog\"\nversion = \"0.1.0\"\n\n[dependencies]\nname = \"wrong\"\n";
        assert_eq!(package_name(manifest).as_deref(), Some("blog"));
    }

    #[test]
    fn declares_a_module_once_and_keeps_it_sorted() {
        let dir = std::env::temp_dir().join(format!("rustlavel-project-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mod_file = dir.join("mod.rs");
        std::fs::write(&mod_file, "pub mod welcome_controller;\n").unwrap();

        assert!(declare_module(&mod_file, "post_controller").unwrap());
        assert!(!declare_module(&mod_file, "post_controller").unwrap());

        let contents = std::fs::read_to_string(&mod_file).unwrap();
        assert_eq!(contents, "pub mod post_controller;\npub mod welcome_controller;\n");
    }
}

#[cfg(test)]
mod declaration_tests {
    use super::declare_module;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("rustlavel-declare-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        dir.join("mod.rs")
    }

    /// The file `make:module` writes into is not a list of declarations: it
    /// holds the `Module` trait and the `all()` list too. Sorting every line —
    /// which is what this used to do — turned it into fragments, and the
    /// project stopped compiling on the first `make:module`.
    #[test]
    fn a_module_file_with_code_in_it_survives() {
        let path = scratch("keeps-code");
        let before = "//! Doc.\n\npub mod backup;\n\nuse rustlavel::Plugin;\n\n\
                      pub trait Module: Plugin {\n    fn settings(&self) -> &'static [u8] {\n\
                      \x20       &[]\n    }\n}\n";
        std::fs::write(&path, before).unwrap();

        assert!(declare_module(&path, "reports").unwrap());
        let after = std::fs::read_to_string(&path).unwrap();

        assert!(after.contains("pub mod backup;"), "{after}");
        assert!(after.contains("pub mod reports;"), "{after}");
        // Everything that was not a declaration is untouched and in order.
        assert!(after.contains("pub trait Module: Plugin {"), "{after}");
        assert!(after.starts_with("//! Doc."), "{after}");
        assert!(
            after.find("pub mod backup;") < after.find("pub mod reports;"),
            "declarations should stay alphabetical: {after}"
        );
        assert!(
            after.find("pub mod reports;") < after.find("use rustlavel::Plugin;"),
            "a declaration must not jump past the code below it: {after}"
        );
    }

    #[test]
    fn declaring_the_same_module_twice_changes_nothing() {
        let path = scratch("twice");
        std::fs::write(&path, "pub mod backup;\n").unwrap();

        assert!(declare_module(&path, "reports").unwrap());
        assert!(!declare_module(&path, "reports").unwrap());
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.matches("pub mod reports;").count(), 1, "{after}");
    }
}
