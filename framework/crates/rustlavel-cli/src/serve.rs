//! `rustlavel serve` — run the application and restart it when a file changes.
//!
//! Rust's compile step is the main cost of a change here, so the watcher exists
//! to make sure that cost is paid once, automatically, instead of the developer
//! switching windows to restart by hand.

use crate::console;
use crate::project::Project;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, SystemTime};

pub fn run(args: &[String]) -> Result<(), String> {
    let project = Project::discover()?;
    let mut forwarded: Vec<String> = Vec::new();
    let mut watch = true;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--no-watch" => watch = false,
            "--port" => {
                let port = iter.next().ok_or("--port needs a value")?;
                // SAFETY: set before the child process is spawned, single-threaded.
                unsafe { std::env::set_var("SERVER_PORT", port) };
            }
            "--host" => {
                let host = iter.next().ok_or("--host needs a value")?;
                unsafe { std::env::set_var("SERVER_HOST", host) };
            }
            other => forwarded.push(other.to_string()),
        }
    }

    console::heading(&format!("Serving {}", console::accent(&project.crate_name)));
    if watch {
        console::info(&console::dim("watching src/, config/, .env — save to reload"));
    }

    let mut child = spawn(&project, &forwarded)?;

    if !watch {
        let status = child.wait().map_err(|e| e.to_string())?;
        return exit_status(status.code());
    }

    let mut fingerprint = snapshot(&project.watched());

    loop {
        std::thread::sleep(Duration::from_millis(400));

        // The app exited on its own — a compile error, a panic at boot, or the
        // developer pressing Ctrl-C, which the child receives too.
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            if status.success() {
                return Ok(());
            }
            console::info(&console::dim("app exited; waiting for a change…"));
            fingerprint = wait_for_change(&project, fingerprint);
            child = spawn(&project, &forwarded)?;
            continue;
        }

        let current = snapshot(&project.watched());
        if current != fingerprint {
            fingerprint = current;
            console::info(&console::dim("change detected, restarting…"));
            let _ = child.kill();
            let _ = child.wait();
            child = spawn(&project, &forwarded)?;
        }
    }
}

fn exit_status(code: Option<i32>) -> Result<(), String> {
    match code {
        Some(0) | None => Ok(()),
        Some(code) => Err(format!("application exited with status {code}")),
    }
}

fn wait_for_change(project: &Project, previous: Fingerprint) -> Fingerprint {
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let current = snapshot(&project.watched());
        if current != previous {
            return current;
        }
    }
}

fn spawn(project: &Project, args: &[String]) -> Result<Child, String> {
    Command::new("cargo")
        .arg("run")
        .arg("--quiet")
        .arg("--bin")
        .arg(&project.crate_name)
        .args(if args.is_empty() { vec![] } else { vec!["--".to_string()] })
        .args(args)
        .current_dir(&project.root)
        .spawn()
        .map_err(|e| format!("cannot run cargo: {e}"))
}

type Fingerprint = BTreeMap<PathBuf, SystemTime>;

/// Modification times of every watched source file.
fn snapshot(paths: &[PathBuf]) -> Fingerprint {
    let mut out = BTreeMap::new();
    for path in paths {
        collect(path, &mut out);
    }
    out
}

fn collect(path: &Path, out: &mut Fingerprint) {
    let Ok(metadata) = std::fs::metadata(path) else { return };

    if metadata.is_file() {
        if is_watchable(path)
            && let Ok(modified) = metadata.modified() {
                out.insert(path.to_path_buf(), modified);
            }
        return;
    }

    let Ok(entries) = std::fs::read_dir(path) else { return };
    for entry in entries.flatten() {
        let child = entry.path();
        // Never descend into build output: it changes on every compile and
        // would restart the app in a loop.
        if child.file_name().is_some_and(|name| name == "target" || name == ".git") {
            continue;
        }
        collect(&child, out);
    }
}

fn is_watchable(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs" | "json" | "toml" | "html" | "css" | "js") => true,
        // `.env` has no extension.
        None => path.file_name().is_some_and(|name| name == ".env"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_output_is_never_watched() {
        let dir = std::env::temp_dir().join("rustlavel-serve-test");
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("target/debug/app"), "binary").unwrap();

        let taken = snapshot(std::slice::from_ref(&dir));

        assert!(taken.keys().any(|p| p.ends_with("src/main.rs")));
        assert!(!taken.keys().any(|p| p.to_string_lossy().contains("target")));
    }

    #[test]
    fn only_source_files_are_watched() {
        assert!(is_watchable(Path::new("src/main.rs")));
        assert!(is_watchable(Path::new("config/app.json")));
        assert!(is_watchable(Path::new(".env")));
        assert!(!is_watchable(Path::new("public/logo.png")));
    }
}
