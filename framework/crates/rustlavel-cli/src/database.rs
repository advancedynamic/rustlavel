//! The database generators: `make:model`, `make:migration`, `make:seeder`.
//!
//! Each writes a file and then regenerates the registry beside it. Laravel
//! discovers migrations by listing a directory at runtime; a compiled program
//! cannot, so the CLI keeps a generated `mod.rs` in step. The developer never
//! edits it, which is what keeps the experience the same.

use crate::console;
use crate::naming;
use crate::project::Project;
use crate::stubs::{self, render};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn model(project: &Project, name: &str) -> Result<(), String> {
    let class = naming::pascal(name);
    let module = naming::snake(&class);
    let table = naming::table_name(&class);

    let mut values = BTreeMap::new();
    values.insert("class", class.clone());
    values.insert("table", table.clone());

    let path = project.root.join("src/models").join(format!("{module}.rs"));
    write_new(&path, &render(stubs::MODEL_STUB, &values))?;
    console::created(&relative(project, &path));

    declare(project, "src/models/mod.rs", &module)?;
    ensure_module(project, "models")?;

    console::success(&format!("{class} created, mapped to the `{table}` table."));
    Ok(())
}

pub fn migration(project: &Project, description: &str) -> Result<(), String> {
    let slug = naming::snake(description);
    let class = naming::pascal(description);
    let name = format!("{}_{slug}", timestamp());
    // `create_posts_table` describes the `posts` table.
    let table = slug
        .strip_prefix("create_")
        .and_then(|rest| rest.strip_suffix("_table"))
        .unwrap_or(&slug)
        .to_string();

    let mut values = BTreeMap::new();
    values.insert("class", class.clone());
    values.insert("name", name.clone());
    values.insert("table", table);
    values.insert("description", description.to_string());

    let path = project.root.join("database/migrations").join(format!("m{name}.rs"));
    write_new(&path, &render(stubs::MIGRATION_STUB, &values))?;
    console::created(&relative(project, &path));

    regenerate_registry(project, "database/migrations", stubs::MIGRATIONS_REGISTRY, "m")?;
    ensure_database_module(project)?;

    console::success(&format!("{name} created. Run `rustlavel migrate` to apply it."));
    Ok(())
}

pub fn seeder(project: &Project, name: &str) -> Result<(), String> {
    let class = naming::pascal(name.trim_end_matches("Seeder")) + "Seeder";
    let module = naming::snake(&class);
    let table = naming::table_name(name.trim_end_matches("Seeder"));

    let mut values = BTreeMap::new();
    values.insert("class", class.clone());
    values.insert("table", table);

    let path = project.root.join("database/seeders").join(format!("{module}.rs"));
    write_new(&path, &render(stubs::SEEDER_STUB, &values))?;
    console::created(&relative(project, &path));

    regenerate_registry(project, "database/seeders", stubs::SEEDERS_REGISTRY, "")?;
    ensure_database_module(project)?;

    console::success(&format!("{class} created. Run `rustlavel db:seed` to run it."));
    Ok(())
}

/// Rewrite the generated registry from whatever files are on disk.
///
/// Deriving it from the directory means a deleted file cannot leave a dangling
/// entry behind, which is the failure mode a hand-maintained list would have.
fn regenerate_registry(
    project: &Project,
    directory: &str,
    template: &str,
    file_prefix: &str,
) -> Result<(), String> {
    let dir = project.root.join(directory);
    let mut modules: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter_map(|path| path.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .filter(|stem| stem != "mod")
        .collect();

    // Names start with a timestamp, so sorting them is sorting by time.
    modules.sort();

    let declarations = modules
        .iter()
        .map(|module| format!("pub mod {module};"))
        .collect::<Vec<_>>()
        .join("\n");

    let entries = modules
        .iter()
        .map(|module| {
            let type_name = type_name_for(module, file_prefix);
            format!("        &{module}::{type_name},")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut values = BTreeMap::new();
    values.insert("modules", declarations);
    values.insert("entries", entries);

    let path = dir.join("mod.rs");
    std::fs::write(&path, render(template, &values))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    console::updated(&relative(project, &path));
    Ok(())
}

/// `m2026_08_29_000001_create_users_table` → `CreateUsersTable`.
fn type_name_for(module: &str, file_prefix: &str) -> String {
    let stripped = module.strip_prefix(file_prefix).unwrap_or(module);

    // Drop a leading `YYYY_MM_DD_HHMMSS_` timestamp if there is one.
    let parts: Vec<&str> = stripped.split('_').collect();
    let descriptive = if parts.len() > 4 && parts[0].len() == 4 && parts[0].chars().all(|c| c.is_ascii_digit())
    {
        parts[4..].join("_")
    } else {
        stripped.to_string()
    };

    naming::pascal(&descriptive)
}

fn declare(project: &Project, mod_path: &str, module: &str) -> Result<(), String> {
    let path = project.root.join(mod_path);
    if crate::project::declare_module(&path, module)? {
        console::updated(&relative(project, &path));
    }
    Ok(())
}

/// Add `pub mod <name>;` to the application's lib.rs if it is not there.
fn ensure_module(project: &Project, name: &str) -> Result<(), String> {
    let lib = project.root.join("src/lib.rs");
    let contents = std::fs::read_to_string(&lib).unwrap_or_default();
    let declaration = format!("pub mod {name};");

    if contents.contains(&declaration) {
        return Ok(());
    }
    std::fs::write(&lib, format!("{contents}{declaration}\n")).map_err(|e| e.to_string())?;
    console::updated(&relative(project, &lib));
    Ok(())
}

/// `database/` sits outside `src/`, so it needs a module that points at it.
fn ensure_database_module(project: &Project) -> Result<(), String> {
    let path = project.root.join("src/database.rs");
    if !path.exists() {
        std::fs::write(
            &path,
            "//! Generated by the rustlavel CLI: bridges `database/` into the crate.\n\
             \n\
             #[path = \"../database/migrations/mod.rs\"]\n\
             pub mod migrations;\n\
             \n\
             #[path = \"../database/seeders/mod.rs\"]\n\
             pub mod seeders;\n",
        )
        .map_err(|e| e.to_string())?;
        console::created(&relative(project, &path));
    }

    // Both directories must exist, since the bridge names them unconditionally.
    for directory in ["database/migrations", "database/seeders"] {
        let dir = project.root.join(directory);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let mod_file = dir.join("mod.rs");
        if !mod_file.exists() {
            let template = if directory.ends_with("migrations") {
                stubs::MIGRATIONS_REGISTRY
            } else {
                stubs::SEEDERS_REGISTRY
            };
            let mut values = BTreeMap::new();
            values.insert("modules", String::new());
            values.insert("entries", String::new());
            std::fs::write(&mod_file, render(template, &values)).map_err(|e| e.to_string())?;
        }
    }

    ensure_module(project, "database")
}

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

/// `YYYY_MM_DD_HHMMSS`, so migration names sort into the order they were made.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let (year, month, day) = civil_from_days(now.div_euclid(86_400));
    let seconds = now.rem_euclid(86_400);

    format!(
        "{year:04}_{month:02}_{day:02}_{:02}{:02}{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// Days since the unix epoch to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

/// Present, so `PathBuf` stays imported for the helpers above.
#[allow(dead_code)]
type _Paths = PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_type_name_from_a_migration_file() {
        assert_eq!(
            type_name_for("m2026_08_29_143000_create_users_table", "m"),
            "CreateUsersTable"
        );
        assert_eq!(type_name_for("user_seeder", ""), "UserSeeder");
    }

    #[test]
    fn timestamps_sort_chronologically() {
        let stamp = timestamp();

        assert_eq!(stamp.len(), "2026_08_29_143000".len());
        assert!(stamp.starts_with("20"));
        // Sorting strings must sort dates: an earlier day compares smaller.
        assert!(*"2026_08_28_235959" < *stamp);
    }
}
