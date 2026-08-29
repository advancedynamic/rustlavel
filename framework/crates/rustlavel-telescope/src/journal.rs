//! Optional JSON-lines persistence, so a restart does not lose the request you
//! were about to look at.
//!
//! Why not SQLite (which is what Laravel Telescope uses)? Two reasons, and both
//! are product decisions rather than shortcuts.
//!
//! *A debugging tool must never be the reason a request is slow.* A database
//! means a schema, a connection, a transaction and an `fsync` on the path of
//! every event — for data whose whole purpose is to be looked at once and
//! thrown away. Here the request's thread does a mutex, a push, and a send into
//! an unbounded queue; a writer thread does the file I/O afterwards. Nothing on
//! the request path can block on a disk.
//!
//! *A debugging tool must never add a dependency to production builds.* An
//! embedded database would be compiled into every application that ever wanted
//! to look at a query locally. Append-only lines of the JSON core already has
//! cost nothing, are readable with `tail -f`, and survive a `kill -9` losing at
//! most the last line.

use crate::entry::Entry;
use rustlavel_core::Json;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};

/// A background writer for recorded entries.
///
/// The handle only ever sends on an unbounded channel, which cannot block and
/// cannot fail in a way worth propagating: if the writer thread is gone, the
/// application still works, it just stops persisting.
pub struct Journal {
    tx: Sender<Entry>,
    path: PathBuf,
}

/// One warning per process is enough. Note this writes to stderr directly and
/// never through `rustlavel_core::log`: a log line dispatches an event, which
/// the recorder would hand straight back to this journal.
static WARNED: AtomicBool = AtomicBool::new(false);

fn warn_once(message: &str) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("telescope: {message}");
    }
}

impl Journal {
    /// Start writing to `path`, creating its parent directory if needed.
    pub fn open(path: impl Into<PathBuf>) -> Journal {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            warn_once(&format!("could not create {}: {error}", parent.display()));
        }

        let (tx, rx) = channel::<Entry>();
        let target = path.clone();

        // A plain thread rather than a tokio task: the plugin registers during
        // boot, which may be outside a runtime, and this work is blocking file
        // I/O that has no business on an async executor anyway.
        let spawned = std::thread::Builder::new()
            .name("telescope-journal".to_string())
            .spawn(move || {
                // Blocks on the channel, so an idle application costs nothing.
                while let Ok(entry) = rx.recv() {
                    let mut batch = vec![entry];
                    // A burst of events becomes one open/write/close.
                    batch.extend(rx.try_iter());
                    append(&target, &batch);
                }
            });

        if let Err(error) = spawned {
            warn_once(&format!("could not start the writer thread: {error}"));
        }

        Journal { tx, path }
    }

    /// Queue an entry. Never blocks, never fails loudly.
    pub fn append(&self, entry: &Entry) {
        let _ = self.tx.send(entry.clone());
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read back the last `capacity` entries and compact the file to them.
    ///
    /// An id may appear more than once: a request only learns which queries
    /// and log lines belong to it once it finishes, so those entries are
    /// appended a second time with their grouping filled in. The last line
    /// written for an id therefore wins, which is also what makes compaction
    /// safe — the file that comes back holds one line per entry, so an
    /// append-only journal can never outgrow the buffer it feeds.
    pub fn load(path: impl AsRef<Path>, capacity: usize) -> Vec<Entry> {
        let path = path.as_ref();
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Vec::new();
        };

        // A BTreeMap both de-duplicates (last line wins) and orders by id.
        let mut latest: BTreeMap<u64, Entry> = BTreeMap::new();
        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            if let Ok(value) = Json::parse(line)
                && let Some(entry) = Entry::from_json(&value)
            {
                latest.insert(entry.id, entry);
            }
        }

        let mut entries: Vec<Entry> = latest.into_values().collect();
        if entries.len() > capacity {
            entries.drain(..entries.len() - capacity);
        }

        let compacted: String =
            entries.iter().map(|entry| format!("{}\n", entry.to_json())).collect();
        if let Err(error) = std::fs::write(path, compacted) {
            warn_once(&format!("could not compact {}: {error}", path.display()));
        }

        entries
    }
}

fn append(path: &Path, entries: &[Entry]) {
    let file = OpenOptions::new().create(true).append(true).open(path);
    let mut file = match file {
        Ok(file) => file,
        Err(error) => {
            warn_once(&format!("could not open {}: {error}", path.display()));
            return;
        }
    };

    let mut buffer = String::new();
    for entry in entries {
        buffer.push_str(&entry.to_json().to_string());
        buffer.push('\n');
    }
    if let Err(error) = file.write_all(buffer.as_bytes()) {
        warn_once(&format!("could not write to {}: {error}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Event;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("rustlavel-telescope-tests");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Wait for the writer thread to catch up, rather than sleeping blindly.
    fn wait_for_lines(path: &Path, expected: usize) -> String {
        for _ in 0..200 {
            if let Ok(contents) = std::fs::read_to_string(path)
                && contents.lines().filter(|l| !l.trim().is_empty()).count() >= expected
            {
                return contents;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("journal never reached {expected} lines");
    }

    #[test]
    fn appended_entries_are_written_as_json_lines() {
        let path = temp_path("appended.jsonl");
        let journal = Journal::open(&path);

        journal.append(&Entry::from_event(1, &Event::new("log").with("message", "one")));
        journal.append(&Entry::from_event(2, &Event::new("log").with("message", "two")));

        let contents = wait_for_lines(&path, 2);
        assert_eq!(contents.lines().count(), 2);
        assert!(contents.contains("\"message\":\"one\""));
    }

    #[test]
    fn entries_are_loaded_back_and_the_file_is_compacted() {
        let path = temp_path("loaded.jsonl");
        let journal = Journal::open(&path);
        for id in 1..=5 {
            journal.append(&Entry::from_event(id, &Event::new("log").with("message", "x")));
        }
        wait_for_lines(&path, 5);

        let loaded = Journal::load(&path, 3);
        assert_eq!(loaded.iter().map(|e| e.id).collect::<Vec<_>>(), vec![3, 4, 5]);

        // The file now holds only what was kept, so it cannot grow unbounded.
        let contents = std::fs::read_to_string(&path).expect("file still exists");
        assert_eq!(contents.lines().count(), 3);
    }

    #[test]
    fn a_corrupt_trailing_line_costs_only_that_line() {
        let path = temp_path("corrupt.jsonl");
        let good = Entry::from_event(1, &Event::new("log").with("message", "kept")).to_json();
        std::fs::write(&path, format!("{good}\n{{\"kind\":\"log\",\n")).expect("write fixture");

        let loaded = Journal::load(&path, 10);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].fields["message"].as_str(), Some("kept"));
    }

    #[test]
    fn loading_a_file_that_does_not_exist_yields_nothing() {
        assert!(Journal::load(temp_path("absent.jsonl"), 10).is_empty());
    }

    #[test]
    fn a_rewritten_entry_replaces_the_line_it_was_first_written_as() {
        let path = temp_path("rewritten.jsonl");
        let journal = Journal::open(&path);

        let mut entry = Entry::from_event(1, &Event::new("db.query").with("sql", "select 1"));
        journal.append(&entry);
        // The same entry again, once a request has claimed it.
        entry.group = Some(9);
        journal.append(&entry);
        wait_for_lines(&path, 2);

        let loaded = Journal::load(&path, 10);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].group, Some(9));
        // Compaction leaves one line per entry, so the file cannot creep up.
        assert_eq!(std::fs::read_to_string(&path).expect("file").lines().count(), 1);
    }
}
