//! `rustlavel upgrade` — bring a project's kit files up to this CLI's version.
//!
//! A starter kit is a hundred files copied into an application, and from the
//! moment they land they are the application's. That is what makes them useful
//! and it is also the problem: a release that fixes one of them cannot reach
//! anybody, because reaching them would mean overwriting whatever they wrote
//! on top. Every project created with 0.5.0 is still running 0.5.0's kit.
//!
//! So this reconciles three versions of every file rather than two:
//!
//! * **base** — what the version in `.rustlavel/manifest.json` wrote, fetched
//!   from crates.io by [`crate::registry`];
//! * **yours** — what is in the project now;
//! * **theirs** — what this CLI would write today.
//!
//! Where only one side moved, that side wins and nobody is asked anything.
//! Where both moved differently, the file is written with conflict markers, so
//! the project stops compiling until a person decides. An upgrade that could
//! not decide must not be able to pass unnoticed — a half-applied upgrade
//! discovered in production is worse than a build that fails now.
//!
//! Two things this deliberately does not do. It does not touch a file the kit
//! never wrote, so your own controllers and views are not in scope at all. And
//! it does not delete: a file that has left the kit is reported, never removed,
//! because the kit cannot know what came to depend on it.

use std::collections::BTreeMap;
use std::path::Path;

use crate::auth_kit;
use crate::console;
use crate::merge;
use crate::naming;
use crate::project::Project;
use crate::registry;
use crate::stubs::render;

/// What the project records about where its kit came from.
///
/// `.rustlavel/manifest.json` is written by `rustlavel new` and rewritten by
/// every successful upgrade. It is meant to be committed: without it there is
/// no base, and without a base there is no merge.
#[cfg_attr(test, derive(Debug))]
struct Manifest {
    kit: String,
    version: String,
}

pub fn run(args: &[String]) -> Result<(), String> {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let allow_dirty = args.iter().any(|a| a == "--allow-dirty");
    let from = flag_value(args, "--from");

    let project = Project::discover()?;
    let manifest = read_manifest(&project.root, from)?;
    let target = env!("CARGO_PKG_VERSION");

    if manifest.version == target {
        console::info(&format!(
            "Already on {target}. Nothing to upgrade."
        ));
        return Ok(());
    }

    if !allow_dirty && !dry_run {
        check_clean(&project.root)?;
    }
    check_no_unfinished_merge(&project.root)?;

    console::heading(&format!(
        "Upgrading the {} kit: {} → {target}",
        manifest.kit, manifest.version
    ));

    let base = registry::base_kit(&manifest.version, &manifest.kit)?;
    let values = values_for(&project);

    let mut clean = 0usize;
    let mut conflicted: Vec<String> = Vec::new();
    let mut untouched = 0usize;
    let mut added: Vec<String> = Vec::new();
    let mut skipped_binary: Vec<String> = Vec::new();
    let mut no_base: Vec<String> = Vec::new();

    for (path, template) in auth_kit::FILES {
        let theirs = render(template, &values);
        let target_path = project.root.join(path);
        let base = base.read(path).map(|text| render(&text, &values));
        let ours = std::fs::read_to_string(&target_path).ok();

        match (base, ours) {
            // New in this version. Nothing to reconcile: it is a file the
            // project has never had an opinion about.
            (_, None) => {
                added.push((*path).to_string());
                if !dry_run {
                    write(&target_path, theirs.as_bytes())?;
                }
            }
            // Present, and you never touched it.
            (Some(base), Some(ours)) if ours == base => {
                if ours == theirs {
                    untouched += 1;
                } else {
                    clean += 1;
                    console::updated(path);
                    if !dry_run {
                        write(&target_path, theirs.as_bytes())?;
                    }
                }
            }
            // Present and edited — and no base to reconcile against, because
            // this version of the kit did not have the file. Both sides
            // invented it, so neither can be preferred.
            (None, Some(ours)) => {
                if ours == theirs {
                    untouched += 1;
                } else {
                    let result = merge::merge("", &ours, &theirs, "yours", &format!("rustlavel {target}"));
                    if result.conflicts == 0 {
                        clean += 1;
                        console::updated(path);
                    } else {
                        no_base.push((*path).to_string());
                    }
                    if !dry_run {
                        write(&target_path, result.text.as_bytes())?;
                    }
                }
            }
            // The case the whole command exists for.
            (Some(base), Some(ours)) => {
                if ours == theirs {
                    untouched += 1;
                } else {
                    let result = merge::merge(&base, &ours, &theirs, "yours", &format!("rustlavel {target}"));
                    record(&mut clean, &mut conflicted, path, &result);
                    if !dry_run {
                        write(&target_path, result.text.as_bytes())?;
                    }
                }
            }
        }
    }

    // Fonts and the like. There is no merging a woff2, so an edited one is
    // reported and left exactly as it is.
    for (path, bytes) in auth_kit::BINARY_FILES {
        let target_path = project.root.join(path);
        let base = base.bytes(path);
        let ours = std::fs::read(&target_path).ok();
        match (base, ours) {
            (_, None) => {
                added.push((*path).to_string());
                if !dry_run {
                    write(&target_path, bytes)?;
                }
            }
            (Some(base), Some(ours)) if ours == base => {
                if ours.as_slice() != *bytes {
                    clean += 1;
                    console::updated(path);
                    if !dry_run {
                        write(&target_path, bytes)?;
                    }
                }
            }
            (_, Some(ours)) => {
                if ours.as_slice() != *bytes {
                    skipped_binary.push((*path).to_string());
                } else {
                    untouched += 1;
                }
            }
        }
    }

    let gone = removed_files(&base, &project.root);
    let env_keys = if dry_run {
        missing_env_keys(&project.root)
    } else {
        add_missing_env_keys(&project.root)?
    };

    let dependency = if dry_run { None } else { update_dependency(&project.root, target)? };

    if !dry_run {
        write_manifest(&project.root, &manifest.kit, target)?;
    }

    report(Report {
        dry_run,
        target,
        clean,
        untouched,
        added,
        conflicted,
        skipped_binary,
        no_base,
        gone,
        dependency,
        env_keys,
    });
    Ok(())
}

fn record(clean: &mut usize, conflicted: &mut Vec<String>, path: &str, result: &merge::Merged) {
    if result.conflicts == 0 {
        *clean += 1;
        console::updated(path);
    } else {
        conflicted.push(format!("{path} ({} conflicts)", result.conflicts));
    }
}

struct Report {
    dry_run: bool,
    target: &'static str,
    clean: usize,
    untouched: usize,
    added: Vec<String>,
    conflicted: Vec<String>,
    skipped_binary: Vec<String>,
    no_base: Vec<String>,
    gone: Vec<String>,
    dependency: Option<String>,
    env_keys: Vec<String>,
}

fn report(r: Report) {
    let verb = if r.dry_run { "would be" } else { "were" };
    println!();
    println!(
        "  {} merged, {} added, {} already current",
        console::bold(&r.clean.to_string()),
        console::bold(&r.added.len().to_string()),
        console::dim(&r.untouched.to_string())
    );

    if !r.added.is_empty() {
        println!("\n  New in {}:", r.target);
        for path in &r.added {
            println!("    {path}");
        }
    }

    if !r.env_keys.is_empty() {
        println!("\n  Settings {verb} appended to .env and .env.example:");
        for key in &r.env_keys {
            println!("    {key}");
        }
    }

    if !r.skipped_binary.is_empty() {
        println!("\n  {}", console::bold("Left alone — you changed these and they cannot be merged:"));
        for path in &r.skipped_binary {
            println!("    {path}");
        }
    }

    if !r.gone.is_empty() {
        println!("\n  {}", console::bold("No longer part of the kit — yours to keep or delete:"));
        for path in &r.gone {
            println!("    {path}");
        }
    }

    if let Some(what) = &r.dependency {
        println!("\n  Cargo.toml: {what}");
    }

    if !r.no_base.is_empty() {
        println!("\n  {}", console::bold("Both versions are in these, because there was nothing to compare against:"));
        for path in &r.no_base {
            println!("    {path}");
        }
        println!(
            "    {}",
            console::dim("the version you upgraded from had no such file, so neither side can be preferred — pick one")
        );
    }

    if r.conflicted.is_empty() && r.no_base.is_empty() {
        if r.dry_run {
            console::info("Nothing would conflict. Run without --dry-run to apply.");
        } else {
            console::success("Upgraded cleanly. Build and run your tests before committing.");
        }
    } else {
        if !r.conflicted.is_empty() {
            println!("\n  {}", console::bold("Both you and the new version changed these:"));
            for entry in &r.conflicted {
                println!("    {entry}");
            }
        }
        if r.dry_run {
            console::info("Run without --dry-run to write them with conflict markers.");
        } else {
            console::info(
                "They are written with <<<<<<< markers, so the project will not build until \
                 you resolve them. That is deliberate.",
            );
        }
    }
}

/// The values every kit template is rendered with.
///
/// These have to be what `new` used, or base and theirs would differ for
/// reasons that have nothing to do with the upgrade. `plugins` is empty on
/// both sides on purpose: whatever a project put there is a change *you* made
/// relative to base, which is exactly how the merge should see it.
fn values_for(project: &Project) -> BTreeMap<&'static str, String> {
    BTreeMap::from([
        ("crate_name", project.crate_name.clone()),
        ("name", project.crate_name.clone()),
        ("app_name", naming::pascal(&project.crate_name)),
        ("plugins", String::new()),
        ("database", String::new()),
        ("dependency", String::new()),
    ])
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Files the old kit had and this one does not.
fn removed_files(base: &registry::Base, root: &Path) -> Vec<String> {
    let known: Vec<&str> = auth_kit::FILES
        .iter()
        .map(|(p, _)| *p)
        .chain(auth_kit::BINARY_FILES.iter().map(|(p, _)| *p))
        .collect();

    base.template_paths()
        .into_iter()
        .filter(|path| !known.contains(&path.as_str()) && root.join(path).exists())
        .collect()
}

/// Keys the kit's `.env` block has that the project's `.env` does not.
///
/// Additive only, and never a rewrite: a value in a project's `.env` is a
/// deployment's own, and an upgrade has no business changing one.
fn missing_env_keys(root: &Path) -> Vec<String> {
    let existing = std::fs::read_to_string(root.join(".env")).unwrap_or_default();
    auth_kit::ENV_ADDITIONS
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, _)| key.trim())
        .filter(|key| !key.is_empty() && !key.starts_with('#'))
        .filter(|key| !existing.lines().any(|line| line.trim_start().starts_with(&format!("{key}="))))
        .map(str::to_string)
        .collect()
}

fn add_missing_env_keys(root: &Path) -> Result<Vec<String>, String> {
    let missing = missing_env_keys(root);
    if missing.is_empty() {
        return Ok(missing);
    }
    let block: String = auth_kit::ENV_ADDITIONS
        .lines()
        .filter(|line| {
            line.split_once('=')
                .map(|(key, _)| missing.iter().any(|m| m == key.trim()))
                .unwrap_or(false)
        })
        .map(|line| format!("{line}\n"))
        .collect();

    for file in [".env", ".env.example"] {
        let path = root.join(file);
        if !path.exists() {
            continue;
        }
        let mut text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("\n# Added by `rustlavel upgrade`.\n{block}"));
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(missing)
}

/// Refuse to start on top of uncommitted work.
///
/// The merge writes over files in place, and the only practical way back is
/// `git checkout`. Requiring a clean tree is what makes that possible; a
/// project outside git is warned rather than blocked, because there is nothing
/// to check and nothing to promise.
fn check_clean(root: &Path) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output();

    let Ok(output) = output else { return Ok(()) };
    if !output.status.success() {
        console::info("Not a git repository, so there is no way back from this. Take a copy first.");
        return Ok(());
    }
    if !String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        return Err("the working tree has uncommitted changes. `rustlavel upgrade` rewrites \
                    files in place, and a clean tree is what lets you undo it with `git \
                    checkout`. Commit or stash first, or pass --allow-dirty."
            .into());
    }
    Ok(())
}

/// Refuse to merge on top of a merge nobody finished.
///
/// The markers left by a previous run are ordinary lines to a three-way merge:
/// it would happily reconcile them into the file and produce something with
/// two generations of markers nested inside each other, which is not something
/// anybody can untangle by reading.
/// Bring `Cargo.toml`'s rustlavel dependency in line with the kit just written.
///
/// Without this an upgrade leaves a project that cannot build, and for a reason
/// it could have fixed: the files are 0.7.2's and the dependency is still
/// 0.5.0's, so the first thing a person sees after a successful merge is a wall
/// of compiler errors about items that do not exist. The kit also grows feature
/// flags over time — 0.5.0 had no `i18n`, and `@lang` needs it — so the
/// features are unioned rather than replaced. Nothing a project added of its
/// own accord is removed.
///
/// A `path = ` dependency is left alone: that is somebody working against a
/// checkout, and a version number would break it.
fn update_dependency(root: &Path, target: &str) -> Result<Option<String>, String> {
    let path = root.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(None) };

    let Some(line) = text.lines().find(|line| line.trim_start().starts_with("rustlavel = {")) else {
        return Ok(None);
    };
    if line.contains("path = ") {
        return Ok(Some("left alone: it points at a local checkout".into()));
    }

    let had: Vec<String> = between(line, "features = [", "]")
        .map(|list| {
            list.split(',')
                .map(|item| item.trim().trim_matches('"').to_string())
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut features = had.clone();
    for required in auth_kit::REQUIRED_PACKAGES {
        if !features.iter().any(|f| f == required) {
            features.push((*required).to_string());
        }
    }
    features.sort();
    features.dedup();

    let from = between(line, "version = \"", "\"").unwrap_or_default().to_string();
    if from == target && features == had {
        return Ok(None);
    }

    let list = features.iter().map(|f| format!("\"{f}\"")).collect::<Vec<_>>().join(", ");
    let replacement = format!("rustlavel = {{ version = \"{target}\", features = [{list}] }}");
    std::fs::write(&path, text.replace(line, &replacement))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    let gained: Vec<&String> = features.iter().filter(|f| !had.contains(f)).collect();
    Ok(Some(if gained.is_empty() {
        format!("{from} → {target}")
    } else {
        format!(
            "{from} → {target}, and gained {}",
            gained.iter().map(|f| f.as_str()).collect::<Vec<_>>().join(", ")
        )
    }))
}

/// The text between two markers on one line.
fn between<'a>(line: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = line.find(open)? + open.len();
    let end = line[start..].find(close)? + start;
    Some(&line[start..end])
}

fn check_no_unfinished_merge(root: &Path) -> Result<(), String> {
    let mut unfinished: Vec<&str> = auth_kit::FILES
        .iter()
        .map(|(path, _)| *path)
        .filter(|path| {
            std::fs::read_to_string(root.join(path))
                .map(|text| merge::has_conflict_markers(&text))
                .unwrap_or(false)
        })
        .collect();
    unfinished.sort();

    if unfinished.is_empty() {
        return Ok(());
    }
    Err(format!(
        "these files still hold conflict markers from an earlier upgrade:\n{}\n  Resolve them \
         first — merging on top would nest one set of markers inside another.",
        unfinished
            .iter()
            .map(|path| format!("    {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn read_manifest(root: &Path, from: Option<String>) -> Result<Manifest, String> {
    let path = root.join(".rustlavel/manifest.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let kit = field(&text, "kit").ok_or_else(|| format!("{} has no \"kit\"", path.display()))?;
            let version = from
                .or_else(|| field(&text, "version"))
                .ok_or_else(|| format!("{} has no \"version\"", path.display()))?;
            Ok(Manifest { kit, version })
        }
        Err(_) => {
            let version = from.ok_or_else(|| {
                format!(
                    "this project has no {}, so there is no record of which version wrote its \
                     kit — projects created before 0.7.2 have none. Pass the version it was \
                     created with, for example `rustlavel upgrade --from 0.5.0`, and one will \
                     be written for next time.",
                    ".rustlavel/manifest.json"
                )
            })?;
            Ok(Manifest { kit: "auth-kit".into(), version })
        }
    }
}

/// Write `.rustlavel/manifest.json`.
///
/// Hand-written rather than serialised: the CLI has no dependencies, and this
/// is three strings.
pub fn write_manifest(root: &Path, kit: &str, version: &str) -> Result<(), String> {
    let path = root.join(".rustlavel/manifest.json");
    let body = format!(
        "{{\n  \"kit\": \"{kit}\",\n  \"version\": \"{version}\"\n}}\n"
    );
    write(&path, body.as_bytes())?;
    Ok(())
}

/// The value of a top-level `"key": "value"` pair.
///
/// A whole JSON parser for a file this CLI writes itself would be a parser
/// nobody needs; anything more elaborate than two strings belongs somewhere
/// else anyway.
fn field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after = text.find(&needle)? + needle.len();
    let rest = text[after..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        // Tests run concurrently, so every one gets a directory of its own.
        let dir = std::env::temp_dir().join(format!("rustlavel-upgrade-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }


    fn cargo_toml(dir: &std::path::Path, line: &str) -> String {
        std::fs::write(dir.join("Cargo.toml"), format!("[package]\nname = \"demo\"\n\n[dependencies]\n{line}\n")).unwrap();
        std::fs::read_to_string(dir.join("Cargo.toml")).unwrap()
    }

    /// The gap this closes: a 0.5.0 project merged 0.7.2's files and then would
    /// not build, because the dependency still said 0.5.0 and the kit's new
    /// `@lang` needs a feature 0.5.0 never enabled.
    #[test]
    fn the_dependency_is_bumped_and_gains_what_the_kit_needs() {
        let dir = temp("dependency");
        cargo_toml(&dir, r#"rustlavel = { version = "0.5.0", features = ["auth", "db", "view"] }"#);

        let what = update_dependency(&dir, "0.7.2").unwrap().expect("something changed");
        assert!(what.contains("0.5.0 → 0.7.2"), "{what}");
        assert!(what.contains("i18n"), "{what}");

        let written = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        for required in auth_kit::REQUIRED_PACKAGES {
            assert!(written.contains(&format!("\"{required}\"")), "{required} is missing:\n{written}");
        }
        assert!(written.contains(r#"version = "0.7.2""#), "{written}");
    }

    /// A feature the project chose is not the kit's to remove.
    #[test]
    fn features_of_your_own_survive() {
        let dir = temp("own-features");
        cargo_toml(&dir, r#"rustlavel = { version = "0.5.0", features = ["auth", "openapi", "ws"] }"#);
        update_dependency(&dir, "0.7.2").unwrap();
        let written = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(written.contains("\"openapi\""), "{written}");
        assert!(written.contains("\"ws\""), "{written}");
    }

    /// Somebody working against a checkout has a path dependency, and writing a
    /// version number over it would point their project at crates.io mid-change.
    #[test]
    fn a_path_dependency_is_never_rewritten() {
        let dir = temp("path-dep");
        let before = cargo_toml(&dir, r#"rustlavel = { path = "../framework/crates/rustlavel", features = ["auth"] }"#);
        let what = update_dependency(&dir, "0.7.2").unwrap().expect("it says why");
        assert!(what.contains("local checkout"), "{what}");
        assert_eq!(std::fs::read_to_string(dir.join("Cargo.toml")).unwrap(), before);
    }

    #[test]
    fn a_project_already_current_is_not_rewritten() {
        let dir = temp("already");
        let list = auth_kit::REQUIRED_PACKAGES
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let before = cargo_toml(&dir, &format!(r#"rustlavel = {{ version = "0.7.2", features = [{list}] }}"#));
        assert!(update_dependency(&dir, "0.7.2").unwrap().is_none());
        assert_eq!(std::fs::read_to_string(dir.join("Cargo.toml")).unwrap(), before);
    }

    #[test]
    fn a_manifest_round_trips() {
        let dir = temp("round-trip");
        write_manifest(&dir, "auth-kit", "0.7.2").unwrap();
        let manifest = read_manifest(&dir, None).unwrap();
        assert_eq!(manifest.kit, "auth-kit");
        assert_eq!(manifest.version, "0.7.2");
    }

    /// `--from` is how a project created before manifests exist gets upgraded,
    /// so it has to win over whatever is on disk.
    #[test]
    fn from_overrides_the_recorded_version() {
        let dir = temp("from-wins");
        write_manifest(&dir, "auth-kit", "0.7.1").unwrap();
        let manifest = read_manifest(&dir, Some("0.5.0".into())).unwrap();
        assert_eq!(manifest.version, "0.5.0");
    }

    /// Without a manifest and without `--from` there is no base, and guessing
    /// one would merge against files the project never had.
    #[test]
    fn a_project_with_no_manifest_is_told_what_to_do() {
        let dir = temp("no-manifest");
        let error = read_manifest(&dir, None).unwrap_err();
        assert!(error.contains("--from 0.5.0"), "{error}");
    }

    #[test]
    fn env_keys_already_present_are_not_offered_again() {
        let dir = temp("env");
        let first = auth_kit::ENV_ADDITIONS
            .lines()
            .find_map(|line| line.split_once('='))
            .map(|(key, _)| key.trim().to_string())
            .expect("the kit adds at least one environment variable");

        std::fs::write(dir.join(".env"), format!("{first}=already-set\n")).unwrap();
        let missing = missing_env_keys(&dir);
        assert!(!missing.contains(&first), "{first} is set but was offered again");
        assert!(!missing.is_empty(), "the kit adds more than one key");
    }

    #[test]
    fn every_env_key_is_missing_from_an_empty_project() {
        let dir = temp("env-empty");
        std::fs::write(dir.join(".env"), "").unwrap();
        assert!(!missing_env_keys(&dir).is_empty());
    }

    #[test]
    fn a_field_is_read_out_of_the_manifest_shape_we_write() {
        let text = "{\n  \"kit\": \"auth-kit\",\n  \"version\": \"0.7.2\"\n}\n";
        assert_eq!(field(text, "kit").as_deref(), Some("auth-kit"));
        assert_eq!(field(text, "version").as_deref(), Some("0.7.2"));
        assert_eq!(field(text, "absent"), None);
    }
}
