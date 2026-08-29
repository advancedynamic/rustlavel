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

    let mut lines: Vec<String> = existing
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    lines.push(declaration);
    lines.sort();

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
        let dir = std::env::temp_dir().join("rustlavel-project-test");
        std::fs::create_dir_all(&dir).unwrap();
        let mod_file = dir.join("mod.rs");
        std::fs::write(&mod_file, "pub mod welcome_controller;\n").unwrap();

        assert!(declare_module(&mod_file, "post_controller").unwrap());
        assert!(!declare_module(&mod_file, "post_controller").unwrap());

        let contents = std::fs::read_to_string(&mod_file).unwrap();
        assert_eq!(contents, "pub mod post_controller;\npub mod welcome_controller;\n");
    }
}
