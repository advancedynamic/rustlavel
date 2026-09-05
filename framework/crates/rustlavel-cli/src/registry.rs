//! The kit as an earlier version of this CLI wrote it.
//!
//! A three-way merge needs the *base*: the files the project started from. The
//! project does not keep them — it records only which version produced them —
//! so they come from where that version is still published, which is
//! crates.io. Every release of `rustlavel-cli` carries `templates/` inside the
//! `.crate` file, so the base of any project is a download away.
//!
//! **Downloading and unpacking is `curl` and `tar`, not code here.** The CLI
//! has no dependencies and is worth keeping that way; giving it an HTTP
//! client, a TLS stack and a gzip decoder so that it can read one archive per
//! upgrade is the wrong trade. Both tools are present anywhere `cargo` is —
//! rustup itself is installed by piping curl to sh — and the CLI already
//! shells out to `cargo` for every forwarded command.
//!
//! Downloads are cached, so upgrading twice, or upgrading after a conflict,
//! costs one round trip in total.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One published version of the CLI, unpacked.
///
/// Two places hold a kit file, because until 0.7.2 they were not the same
/// place: most of the kit has always been a file under `templates/`, but eight
/// of them — `main.rs`, the two registries, the seeder, `lib.rs` and the three
/// config files — were Rust constants inside `src/auth_kit.rs`, written to a
/// project without ever being files. `read` looks in both, so a project
/// created before that changed still has a base for them and gets a merge
/// rather than a conflict over a file it never touched.
pub struct Base {
    templates: PathBuf,
    source: PathBuf,
}

impl Base {
    /// The kit's copy of `path`, wherever this version kept it.
    pub fn read(&self, path: &str) -> Option<String> {
        if let Ok(text) = std::fs::read_to_string(self.templates.join(path)) {
            return Some(text);
        }
        let name = LEGACY_CONSTANTS.iter().find(|(p, _)| *p == path)?.1;
        constant(&std::fs::read_to_string(&self.source).ok()?, name)
    }

    pub fn bytes(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.templates.join(path)).ok()
    }

    /// Every path this version's `templates/` holds, relative to the kit root.
    pub fn template_paths(&self) -> Vec<String> {
        let mut found = Vec::new();
        walk(&self.templates, &self.templates, &mut found);
        found.sort();
        found
    }
}

/// The files that used to be constants, and the constant each one was.
///
/// `src/lib.rs` is absent on purpose: older versions composed it in `new.rs`
/// with `format!` rather than keeping it whole anywhere, so there is nothing
/// honest to read back. It is six `pub mod` lines and resolves in seconds.
const LEGACY_CONSTANTS: &[(&str, &str)] = &[
    ("src/main.rs", "MAIN_RS"),
    ("database/migrations/mod.rs", "MIGRATIONS_REGISTRY"),
    ("database/seeders/mod.rs", "SEEDERS_REGISTRY"),
    ("database/seeders/auth_kit_seeder.rs", "SEEDER"),
    ("config/auth.json", "CONFIG_AUTH"),
    ("config/rbac.json", "CONFIG_RBAC"),
    ("config/webauthn.json", "CONFIG_WEBAUTHN"),
];

/// The body of `pub const NAME: &str = r#"…"#;` in Rust source.
///
/// Reading one constant out of a file is not parsing Rust, and it does not
/// have to be: these are raw strings written by this project, in one shape,
/// and a version that used a different shape is a version that does not match
/// and returns nothing.
fn constant(source: &str, name: &str) -> Option<String> {
    let opening = format!("pub const {name}: &str = r#\"");
    let start = source.find(&opening)? + opening.len();
    let end = source[start..].find("\"#;")?;
    Some(source[start..start + end].to_string())
}

fn walk(root: &Path, dir: &Path, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, found);
        } else if let Ok(relative) = path.strip_prefix(root)
            && let Some(text) = relative.to_str()
        {
            found.push(text.to_string());
        }
    }
}

/// Where a published version's templates live once they are local.
///
/// Returns the directory holding that version's copy of `kit` — the same shape
/// as `templates/<kit>/` in this repository, so a path that works against one
/// works against the other.
pub fn base_kit(version: &str, kit: &str) -> Result<Base, String> {
    let unpacked = cache_root()?.join(format!("rustlavel-cli-{version}"));
    let templates = unpacked.join("templates").join(kit);

    let source = unpacked.join("src").join("auth_kit.rs");
    if templates.is_dir() {
        return Ok(Base { templates, source });
    }

    let parent = unpacked
        .parent()
        .ok_or_else(|| "the cache directory has no parent".to_string())?
        .to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;

    let archive = parent.join(format!("rustlavel-cli-{version}.crate"));
    if !archive.is_file() {
        download(version, &archive)?;
    }
    unpack(&archive, &parent)?;

    if !templates.is_dir() {
        return Err(format!(
            "rustlavel-cli {version} does not carry a `{kit}` kit. The project says it was \
             created with one, so either the version in .rustlavel/manifest.json is wrong or \
             the kit was renamed."
        ));
    }
    Ok(Base { templates, source })
}

fn download(version: &str, into: &Path) -> Result<(), String> {
    let url = format!("https://static.crates.io/crates/rustlavel-cli/rustlavel-cli-{version}.crate");

    let output = Command::new("curl")
        .args(["--proto", "=https", "--tlsv1.2", "--location", "--fail", "--silent", "--show-error"])
        .arg("--output")
        .arg(into)
        .arg(&url)
        .output()
        .map_err(|e| format!("cannot run curl: {e}. `rustlavel upgrade` needs it to fetch the version this project was created with."))?;

    if !output.status.success() {
        // A failed curl leaves a truncated file behind, and a truncated
        // archive is worse than none: the next run would try to unpack it.
        let _ = std::fs::remove_file(into);
        let why = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "could not download rustlavel-cli {version} from crates.io: {}\n  {url}",
            why.trim()
        ));
    }
    Ok(())
}

fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .output()
        .map_err(|e| format!("cannot run tar: {e}. `rustlavel upgrade` needs it to unpack the version this project was created with."))?;

    if !output.status.success() {
        return Err(format!(
            "could not unpack {}: {}",
            archive.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// `$XDG_CACHE_HOME/rustlavel/base`, or `~/.cache/rustlavel/base`.
///
/// A cache rather than anything in the project: the base belongs to a version
/// of the CLI, not to one application, and two projects created with the same
/// version share it.
fn cache_root() -> Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir).join("rustlavel").join("base"));
    }
    let home = std::env::var("HOME")
        .map_err(|_| "neither HOME nor XDG_CACHE_HOME is set, so there is nowhere to cache the download".to_string())?;
    Ok(PathBuf::from(home).join(".cache").join("rustlavel").join("base"))
}

#[cfg(test)]
mod tests {
    use super::*;


    /// A path in `LEGACY_CONSTANTS` that the kit no longer writes reconstructs
    /// a base for a file nobody wants, and — worse — a *renamed* kit file
    /// silently stops having a base at all, which turns a clean merge into a
    /// conflict for everybody upgrading from an older version.
    #[test]
    fn every_legacy_constant_names_a_file_the_kit_still_writes() {
        for (path, name) in LEGACY_CONSTANTS {
            assert!(
                crate::auth_kit::FILES.iter().any(|(p, _)| p == path),
                "`{path}` is reconstructed from the old `{name}` constant, but the kit no \
                 longer writes it. Either the path changed and this entry has to follow, or \
                 the entry should go."
            );
        }
    }

    #[test]
    fn a_constant_is_read_out_of_rust_source() {
        let source = "//! A file.\npub const SEEDER: &str = r#\"one\ntwo\n\"#;\n\npub const OTHER: &str = r#\"x\"#;\n";
        assert_eq!(constant(source, "SEEDER").as_deref(), Some("one\ntwo\n"));
        assert_eq!(constant(source, "OTHER").as_deref(), Some("x"));
        assert_eq!(constant(source, "ABSENT"), None);
    }

    /// A version whose source does not hold the constant in this shape must
    /// return nothing rather than a truncated guess: a wrong base merges
    /// silently, and a missing one only conflicts.
    #[test]
    fn a_constant_in_an_unfamiliar_shape_is_not_guessed_at() {
        assert_eq!(constant("pub const SEEDER: &str = \"plain\";", "SEEDER"), None);
        assert_eq!(constant("pub const SEEDER: &str = r#\"unterminated", "SEEDER"), None);
    }

    #[test]
    fn the_cache_lives_under_xdg_when_it_is_set() {
        // Process-wide state, so this test owns it and puts it back.
        let before = std::env::var("XDG_CACHE_HOME").ok();
        unsafe { std::env::set_var("XDG_CACHE_HOME", "/tmp/some-cache") };
        assert_eq!(cache_root().unwrap(), PathBuf::from("/tmp/some-cache/rustlavel/base"));
        match before {
            Some(value) => unsafe { std::env::set_var("XDG_CACHE_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
    }

    /// An empty `XDG_CACHE_HOME` is not a directory named "", and treating it
    /// as one would put the cache at `/rustlavel/base`.
    #[test]
    fn an_empty_xdg_falls_back_to_home() {
        let before = std::env::var("XDG_CACHE_HOME").ok();
        unsafe { std::env::set_var("XDG_CACHE_HOME", "") };
        let root = cache_root().unwrap();
        assert!(root.starts_with(std::env::var("HOME").unwrap()), "{root:?}");
        match before {
            Some(value) => unsafe { std::env::set_var("XDG_CACHE_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
    }
}
