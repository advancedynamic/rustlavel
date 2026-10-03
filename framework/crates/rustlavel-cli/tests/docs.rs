//! The documentation may only call methods that exist.
//!
//! **Why this is a test.** The first example in the guide's database section
//! called `where_eq`, passed a string to `order_by`, and called `get()` with no
//! database. None of that exists; it could never have compiled, and it sat at
//! the top of the page people read first until somebody happened to look. Prose
//! is checked by being read; code in prose is checked by nobody, so it drifts.
//!
//! This catches the one drift it can catch cheaply: a method *name* in a code
//! block that no function in the framework defines. It does not check
//! arguments — `order_by("created_at", "desc")` would pass, because `order_by`
//! exists — and so it is the floor, not the guarantee. The guarantee is
//! compiling the snippets, which the recipes added to the cookbook were, once,
//! by hand; this keeps the cheap half honest in between.
//!
//! No regex crate: this repository does not take a dependency for something a
//! page of string scanning does, and a test that needs one is a test somebody
//! eventually deletes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Methods that are legitimately called in examples and defined by nobody here:
/// the standard library, Tokio, and the conventions of the prelude.
const NOT_OURS: &[&str] = &[
    "all", "and_then", "any", "as_bytes", "as_ref", "as_str", "assert", "assert_eq", "await", "build",
    "bytes", "chain", "clone", "cloned", "close", "collect", "contains", "copied", "count",
    "dedup", "default", "ends_with", "entry", "enumerate", "eprintln", "expect", "extend",
    "filter_map", "find", "first", "flush", "fold", "format", "from", "get", "insert", "into",
    "err", "is_empty", "is_err", "is_none", "is_ok", "is_some", "iter", "ok", "join", "keys", "last", "len", "lines", "lock", "map", "map_err", "max",
    "min", "next", "ok_or", "ok_or_else", "or_default", "or_insert", "parse", "position",
    "println", "push", "read", "recv", "remove", "replace", "rev", "run", "send", "sleep",
    "sort", "sort_by", "spawn", "split", "starts_with", "sum", "take", "to_lowercase",
    "to_owned", "to_string", "to_uppercase", "to_vec", "trim", "unwrap", "unwrap_or",
    "unwrap_or_default", "unwrap_or_else", "values", "with_capacity", "write", "zip",
];

fn repository_root() -> Option<PathBuf> {
    // framework/crates/rustlavel-cli  →  the repository is three levels up.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    root.join("docs").is_dir().then_some(root)
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != "templates" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every identifier that follows `fn ` anywhere in the framework.
fn defined_functions(root: &Path) -> BTreeSet<String> {
    let mut files = Vec::new();
    rust_files(&root.join("framework/crates"), &mut files);

    let mut names = BTreeSet::new();
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else { continue };
        let mut rest = text.as_str();
        while let Some(at) = rest.find("fn ") {
            let after = &rest[at + 3..];
            let ident: String =
                after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if !ident.is_empty() {
                names.insert(ident);
            }
            rest = after;
        }
    }
    names
}

fn unescape(code: &str) -> String {
    code.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
}

/// The Rust-looking `<pre><code>` blocks of a page. Shell, JSON and config are
/// left alone; a block counts as Rust if it has a keyword or type that only
/// Rust examples contain.
fn rust_blocks(page: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut rest = page;
    while let Some(open) = rest.find("<pre><code") {
        let after = &rest[open..];
        let Some(start) = after.find('>') else { break };
        let Some(end) = after.find("</code></pre>") else { break };
        let block = unescape(&after[start + 1..end]);
        let looks_like_rust = ["let ", "fn ", "use ", "async ", ".await", "App::", "Router", "req."]
            .iter()
            .any(|marker| block.contains(marker));
        if looks_like_rust {
            blocks.push(block);
        }
        rest = &after[end + "</code></pre>".len()..];
    }
    blocks
}

/// Every `.name(` in a block.
fn method_calls(code: &str) -> BTreeSet<String> {
    let bytes = code.as_bytes();
    let mut names = BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            let mut j = i + 1;
            if j < bytes.len() && (bytes[j].is_ascii_lowercase() || bytes[j] == b'_') {
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let mut k = j;
                while k < bytes.len() && bytes[k] == b' ' {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b'(' {
                    names.insert(code[i + 1..j].to_string());
                }
            }
        }
        i += 1;
    }
    names
}

#[test]
fn every_method_the_documentation_calls_exists_somewhere_in_the_framework() {
    let Some(root) = repository_root() else {
        // A published crate has no docs/ beside it. Said out loud, like every
        // other suite here that cannot run.
        println!("skipped: the repository's docs/ directory is not beside this crate");
        return;
    };

    let defined = defined_functions(&root);
    assert!(
        defined.len() > 1000,
        "found only {} functions in the framework — the scan is broken, and a scan that finds \
         nothing would pass every page",
        defined.len()
    );

    let mut unknown: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut blocks_checked = 0;
    for page in ["guide.html", "cookbook.html", "index.html"] {
        let Ok(html) = fs::read_to_string(root.join("docs").join(page)) else { continue };
        for block in rust_blocks(&html) {
            blocks_checked += 1;
            for name in method_calls(&block) {
                if !defined.contains(&name) && !NOT_OURS.contains(&name.as_str()) {
                    unknown.entry(name).or_default().insert(page.to_string());
                }
            }
        }
    }

    assert!(blocks_checked > 10, "only {blocks_checked} code blocks were checked; the scan is broken");
    assert!(
        unknown.is_empty(),
        "the documentation calls methods that no function in the framework defines: {unknown:?}. \
         Fix the example — or, if it is a standard-library method, add it to NOT_OURS."
    );
}
