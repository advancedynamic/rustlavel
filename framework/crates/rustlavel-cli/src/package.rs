//! `rustlavel make:package <name>` — scaffolding for a third-party package.
//!
//! The framework's own rule is that a package is a crate plus a feature flag on
//! the meta-crate, never an addition to core. Somebody writing a package
//! outside this repository has no way to know that, so the generator writes it
//! down: the README says it in words, and the crate it sits beside says it in
//! code — one `Plugin`, one `from_config` reading its own prefix, one prelude.
//!
//! This is the only generator that does not need to be run inside an
//! application, because a package is not part of one.

use crate::console;
use crate::naming;
use crate::stubs::{self, render};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn run(args: &[String]) -> Result<(), String> {
    let mut name: Option<String> = None;
    for argument in args {
        if argument.starts_with('-') {
            return Err(format!("unknown option `{argument}` for make:package"));
        }
        name = Some(argument.clone());
    }
    let name = name.ok_or("usage: rustlavel make:package <name>")?;

    let crate_name = naming::kebab(&name);
    if crate_name.is_empty() {
        return Err(format!("`{name}` is not a package name."));
    }

    // The prefix is what the package owns in configuration, in the plugin's
    // name, and in the feature flag it would get if it ever moved into the
    // framework — so `rustlavel-audit` and `audit` configure the same way.
    let prefix = naming::snake(crate_name.strip_prefix("rustlavel-").unwrap_or(&crate_name));
    let struct_name = naming::pascal(&prefix);
    let lib_name = naming::snake(&crate_name);

    let root = PathBuf::from(&crate_name);
    if root.exists() {
        return Err(format!("`{crate_name}` already exists"));
    }

    let mut values = BTreeMap::new();
    values.insert("crate_name", crate_name.clone());
    values.insert("lib_name", lib_name.clone());
    values.insert("struct_name", struct_name.clone());
    values.insert("config_prefix", prefix.clone());
    values.insert("route", format!("/{}", naming::kebab(&prefix)));
    values.insert("version", env!("CARGO_PKG_VERSION").to_string());
    values.insert(
        "description",
        format!("A rustlavel package: {}", prefix.replace('_', " ")),
    );

    console::heading(&format!("Creating package {}", console::accent(&crate_name)));

    // `Cargo.toml` and `src/lib.rs` are written together, never one then the
    // other: a crate directory holding a manifest with no target makes every
    // workspace that contains it fail to load.
    let files: &[(&str, &str)] = &[
        ("Cargo.toml", stubs::PACKAGE_CARGO_TOML),
        ("src/lib.rs", stubs::PACKAGE_LIB_RS),
        ("README.md", stubs::PACKAGE_README),
        (".gitignore", stubs::PACKAGE_GITIGNORE),
    ];
    for (path, template) in files {
        write(&root.join(path), &render(template, &values))?;
        console::created(&format!("{crate_name}/{path}"));
    }

    console::success(&format!(
        "{crate_name} created.\n\n  cd {crate_name}\n  cargo test\n\n  \
         An application enables it with one line in main.rs:\n\n    \
         .plugin({struct_name}::from_config(&config))\n\n  \
         It reads `{prefix}.*` and nothing else. README.md has the rest."
    ));
    Ok(())
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four values every template reads, derived the way `run` derives them.
    fn values_for(name: &str) -> BTreeMap<&'static str, String> {
        let crate_name = naming::kebab(name);
        let prefix = naming::snake(crate_name.strip_prefix("rustlavel-").unwrap_or(&crate_name));
        let mut values = BTreeMap::new();
        values.insert("crate_name", crate_name.clone());
        values.insert("lib_name", naming::snake(&crate_name));
        values.insert("struct_name", naming::pascal(&prefix));
        values.insert("config_prefix", prefix.clone());
        values.insert("route", format!("/{}", naming::kebab(&prefix)));
        values.insert("version", env!("CARGO_PKG_VERSION").to_string());
        values.insert("description", format!("A rustlavel package: {}", prefix.replace('_', " ")));
        values
    }

    #[test]
    fn the_prefix_is_the_name_without_the_framework_prefix() {
        let values = values_for("rustlavel-audit-log");

        assert_eq!(values["crate_name"], "rustlavel-audit-log");
        assert_eq!(values["lib_name"], "rustlavel_audit_log");
        assert_eq!(values["config_prefix"], "audit_log");
        assert_eq!(values["struct_name"], "AuditLog");
        assert_eq!(values["route"], "/audit-log");
    }

    #[test]
    fn a_package_outside_the_framework_keeps_its_own_name() {
        let values = values_for("Audit");

        assert_eq!(values["crate_name"], "audit");
        assert_eq!(values["config_prefix"], "audit");
        assert_eq!(values["struct_name"], "Audit");
    }

    #[test]
    fn every_file_renders_with_no_placeholder_left() {
        let values = values_for("rustlavel-audit");

        for (label, template) in [
            ("Cargo.toml", stubs::PACKAGE_CARGO_TOML),
            ("src/lib.rs", stubs::PACKAGE_LIB_RS),
            ("README.md", stubs::PACKAGE_README),
        ] {
            let rendered = render(template, &values);
            assert!(
                !rendered.contains("{{"),
                "{label} still holds a placeholder:\n{rendered}"
            );
        }
    }

    #[test]
    fn the_manifest_carries_what_cargo_publish_demands() {
        let manifest = render(stubs::PACKAGE_CARGO_TOML, &values_for("rustlavel-audit"));

        assert!(manifest.contains("name = \"rustlavel-audit\""));
        assert!(manifest.contains("version = \"0.1.0\""));
        assert!(manifest.contains("description = "));
        assert!(manifest.contains("license = "));
        assert!(manifest.contains("repository = "));
        assert!(manifest.contains("readme = "));
        // Never the meta-crate: that is the cycle the feature flag exists to avoid.
        assert!(!manifest.contains("\nrustlavel = "), "{manifest}");
    }

    #[test]
    fn the_library_has_a_plugin_a_from_config_a_prelude_and_a_test() {
        let lib = render(stubs::PACKAGE_LIB_RS, &values_for("rustlavel-audit"));

        assert!(lib.contains("impl Plugin for Audit"));
        assert!(lib.contains("fn name(&self) -> &'static str"));
        assert!(lib.contains("fn register(self: Box<Self>, setup: &mut Setup<'_>)"));
        assert!(lib.contains("pub fn from_config(config: &Config) -> Audit"));
        assert!(lib.contains("config.bool(\"audit.enabled\", true)"));
        assert!(lib.contains("config.string(\"audit.path\""));
        assert!(lib.contains("pub mod prelude"));
        assert!(lib.contains("#[cfg(test)]"));
        assert!(lib.contains("fn reads_only_its_own_configuration()"));
    }

    #[test]
    fn the_readme_states_the_feature_flag_convention() {
        let readme = render(stubs::PACKAGE_README, &values_for("rustlavel-audit"));

        assert!(readme.contains("a crate plus a feature flag"));
        assert!(readme.contains("never\nan addition to `rustlavel-core`"));
        assert!(readme.contains("audit = [\"dep:rustlavel-audit\"]"));
        assert!(readme.contains("no auto-discovery"));
    }
}
