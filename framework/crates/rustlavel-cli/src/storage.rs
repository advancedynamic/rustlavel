//! `rustlavel storage:link` — make stored files reachable over HTTP.
//!
//! The problem it solves is the one Laravel's `storage:link` solves. Uploads
//! belong under `storage/app`, which is outside the web root on purpose: a
//! directory the server hands out verbatim is a directory nobody should be able
//! to write arbitrary files into. But *some* of those files — avatars, exported
//! documents — are meant to be public.
//!
//! Copying them into `public/` means two copies that drift. Serving
//! `storage/app` directly means publishing the private files beside the public
//! ones. A symlink from `public/storage` to `storage/app/public` gives one copy
//! of the file, one directory that is public, and the rest of `storage/app`
//! still out of reach.

use crate::console;
use crate::project::Project;
use std::path::{Path, PathBuf};

/// Where the link is made, and what it points at.
const LINK: &str = "public/storage";
const TARGET: &str = "storage/app/public";

pub fn link(project: &Project, args: &[String]) -> Result<(), String> {
    let force = args.iter().any(|a| a == "--force" || a == "-f");

    let root = project.root.as_path();
    let link = root.join(LINK);
    let target = root.join(TARGET);

    // The target is created rather than demanded. A fresh project has no
    // `storage/app/public` yet, and failing here would just be a second command
    // to run before this one works.
    std::fs::create_dir_all(&target)
        .map_err(|e| format!("cannot create {}: {e}", display(&target, root)))?;

    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", display(parent, root)))?;
    }

    match existing(&link) {
        // Already pointing where it should. Saying so and stopping is the right
        // answer for a command people run again after every deploy.
        Existing::LinkTo(current) if same_place(&current, &target) => {
            console::success(&format!("{LINK} already points at {TARGET}"));
            return Ok(());
        }
        Existing::LinkTo(current) if !force => {
            return Err(format!(
                "{LINK} is already a link, but it points at {}.\n  \
                 Run `rustlavel storage:link --force` to repoint it.",
                current.display()
            ));
        }
        Existing::LinkTo(_) => {
            std::fs::remove_file(&link)
                .map_err(|e| format!("cannot replace {LINK}: {e}"))?;
        }
        // A real directory is somebody's files. Removing it because a flag was
        // passed is not a risk this command gets to take on their behalf.
        Existing::Directory => {
            return Err(format!(
                "{LINK} is a real directory, not a link, so it may hold files this \
                 command did not put there.\n  \
                 Move or delete it yourself, then run this again — `--force` \
                 deliberately does not cover this case."
            ));
        }
        Existing::File => {
            return Err(format!("{LINK} exists and is a file. Move it out of the way first."));
        }
        Existing::Nothing => {}
    }

    symlink(&target, &link)?;
    console::success(&format!(
        "Linked {LINK} to {TARGET}.\n\n  Files written to the `public` disk are now served \
         from /storage/…"
    ));
    Ok(())
}

enum Existing {
    Nothing,
    /// A symlink, and where it resolves to.
    LinkTo(PathBuf),
    Directory,
    File,
}

/// What is at the path — checked without following the link.
///
/// `Path::exists` follows symlinks, so a link pointing at something deleted
/// reads as "nothing there" and the next step would try to create it again.
fn existing(path: &Path) -> Existing {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Existing::Nothing;
    };

    if metadata.file_type().is_symlink() {
        return Existing::LinkTo(std::fs::read_link(path).unwrap_or_default());
    }
    if metadata.is_dir() {
        return Existing::Directory;
    }
    Existing::File
}

/// Whether a link's target is the directory we would link to.
///
/// Compared after canonicalising, because the stored target may be relative,
/// may contain `..`, or may spell the same directory a different way.
fn same_place(current: &Path, target: &Path) -> bool {
    match (current.canonicalize(), target.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        // A link pointing at something that no longer exists is not the same
        // place, whatever it says.
        _ => false,
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| format!("cannot link {LINK} to {TARGET}: {e}"))
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> Result<(), String> {
    // Windows needs either Developer Mode or an elevated prompt for this, and
    // the error it gives otherwise says nothing useful about which.
    std::os::windows::fs::symlink_dir(target, link).map_err(|e| {
        format!(
            "cannot link {LINK} to {TARGET}: {e}\n  \
             Windows only permits creating a directory symlink with Developer Mode \
             turned on, or from an elevated prompt."
        )
    })
}

fn display(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway project tree, named after the test so two can run at once.
    fn project(name: &str) -> (tempdir::Guard, Project) {
        let guard = tempdir::create(name);
        std::fs::create_dir_all(guard.path().join("public")).unwrap();
        std::fs::write(guard.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let project =
            Project { root: guard.path().to_path_buf(), crate_name: "x".to_string() };
        (guard, project)
    }

    #[test]
    fn creates_the_link_and_the_directory_it_points_at() {
        let (guard, project) = project("creates");

        link(&project, &[]).expect("the link should be made");

        let path = guard.path().join(LINK);
        assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert!(guard.path().join(TARGET).is_dir(), "the target is created, not demanded");
    }

    #[test]
    fn running_it_twice_is_not_an_error() {
        // People run this after every deploy. Failing the second time would
        // make it useless in a script.
        let (_guard, project) = project("twice");

        link(&project, &[]).expect("first");
        link(&project, &[]).expect("second");
    }

    #[test]
    fn a_file_written_through_the_link_is_reachable_from_public() {
        // The whole point, asserted rather than assumed.
        let (guard, project) = project("reachable");
        link(&project, &[]).unwrap();

        std::fs::write(guard.path().join(TARGET).join("avatar.png"), b"pixels").unwrap();

        let served = guard.path().join(LINK).join("avatar.png");
        assert_eq!(std::fs::read(served).unwrap(), b"pixels");
    }

    #[test]
    fn a_link_pointing_somewhere_else_is_refused_until_forced() {
        let (guard, project) = project("elsewhere");
        let elsewhere = guard.path().join("somewhere-else");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(guard.path().join("public")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, guard.path().join(LINK)).unwrap();

        let error = link(&project, &[]).unwrap_err();
        assert!(error.contains("--force"), "the error must say the way out: {error}");

        link(&project, &["--force".to_string()]).expect("force should repoint it");
        assert!(same_place(
            &std::fs::read_link(guard.path().join(LINK)).unwrap(),
            &guard.path().join(TARGET)
        ));
    }

    #[test]
    fn a_real_directory_is_never_removed_even_with_force() {
        // It may hold somebody's files. `--force` covers repointing a link this
        // command made, not deleting a directory it did not.
        let (guard, project) = project("real-directory");
        let occupied = guard.path().join(LINK);
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("theirs.txt"), b"do not delete me").unwrap();

        for arguments in [vec![], vec!["--force".to_string()]] {
            let error = link(&project, &arguments).unwrap_err();
            assert!(error.contains("real directory"), "got {error}");
        }
        assert!(occupied.join("theirs.txt").exists(), "their file survived");
    }

    #[test]
    fn a_broken_link_is_repointed_rather_than_reported_as_missing() {
        // `exists()` follows symlinks, so a link to a deleted directory reads as
        // nothing being there — and creating it again then fails with "file
        // exists", which explains nothing.
        let (guard, project) = project("broken");
        std::os::unix::fs::symlink(guard.path().join("gone"), guard.path().join(LINK)).unwrap();

        let error = link(&project, &[]).unwrap_err();
        assert!(error.contains("--force"), "got {error}");
        link(&project, &["--force".to_string()]).expect("force repoints a broken link");
    }

    /// A directory that removes itself, so the tests leave nothing behind.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct Guard(PathBuf);

        impl Guard {
            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// Named after the test, because these run concurrently and a shared
        /// fixture directory is the flake rule six exists to prevent.
        pub fn create(name: &str) -> Guard {
            let path = std::env::temp_dir().join(format!("rustlavel-storage-link-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("a temporary directory");
            Guard(path)
        }
    }
}
