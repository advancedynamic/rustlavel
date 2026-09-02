//! `rustlavel tinker` — a scratchpad over the compiled application.
//!
//! Laravel's `tinker` is a REPL because PHP is an interpreter: a line is
//! evaluated inside the process that is already running, so `$user = ...` is
//! still there on the next line. None of that is available here, and pretending
//! otherwise would be the wrong kind of magic.
//!
//! What this does instead, plainly: it keeps a scratch crate in `target/tinker`
//! that depends on the application by path, drops the snippet into a `main`
//! wrapped in `#[rustlavel::main]`, compiles it, and runs it. So:
//!
//! - **Every snippet costs a compile.** The first one is slow, because the
//!   scratch crate has never been built. The ones after it reuse the same
//!   crate and the application's own `target/`, so incremental compilation
//!   usually gets them to about a second. The time is printed, every time,
//!   rather than left to feel like a hang.
//! - **Nothing carries between snippets.** No variables, no open connections,
//!   no history of what the last one did. Each snippet is a whole program.
//!
//! That makes it a scratchpad — write the five lines you want to run, run them,
//! read the output — rather than a conversation.

use crate::console;
use crate::naming;
use crate::project::Project;
use crate::stubs::{self, render};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn run(args: &[String]) -> Result<(), String> {
    let mut snippet: Option<String> = None;
    let mut iter = args.iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "-e" | "--execute" => {
                snippet = Some(iter.next().ok_or("-e needs a snippet, as in -e '1 + 1'")?.clone());
            }
            "-h" | "--help" => {
                help();
                return Ok(());
            }
            other => return Err(format!("unknown option `{other}` for tinker")),
        }
    }

    let project = Project::discover()?;
    let scratch = Scratch::prepare(&project)?;

    if let Some(snippet) = snippet {
        return scratch.evaluate(&snippet).map(|_| ());
    }

    // A pipe is a script, not a person: read the whole thing and run it once.
    if !std::io::stdin().is_terminal() {
        let source = std::io::read_to_string(std::io::stdin())
            .map_err(|e| format!("cannot read the snippet from stdin: {e}"))?;
        if source.trim().is_empty() {
            return Err("nothing on stdin to run.".into());
        }
        return scratch.evaluate(&source).map(|_| ());
    }

    repl(&scratch)
}

fn help() {
    console::heading("rustlavel tinker");
    console::info("A scratchpad over your application, not a REPL.\n");
    console::info("  rustlavel tinker                 an interactive loop, one snippet a line");
    console::info("  rustlavel tinker -e '<code>'     run one snippet and leave");
    console::info("  echo '<code>' | rustlavel tinker run what is piped in");
    println!();
    console::info(&console::dim(
        "Every snippet is compiled and run as a program, in a scratch crate kept in\n  \
         target/tinker. State does not carry over: no variables, no connections.",
    ));
    println!();
}

/// The interactive loop.
fn repl(scratch: &Scratch) -> Result<(), String> {
    banner();

    let stdin = std::io::stdin();
    loop {
        print!("{} ", console::accent("»"));
        std::io::stdout().flush().ok();

        let mut line = String::new();
        match stdin.read_line(&mut line) {
            // End of input: ctrl-D, and the same as `:q`.
            Ok(0) => {
                println!();
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => return Err(format!("cannot read input: {e}")),
        }

        match line.trim() {
            "" => continue,
            ":q" | ":quit" | "quit" | "exit" => return Ok(()),
            ":help" | "help" | "?" => {
                help();
                continue;
            }
            snippet => {
                // A snippet that will not compile is not a reason to leave; the
                // compiler has already said what is wrong.
                let _ = scratch.evaluate(snippet);
            }
        }
    }
}

fn banner() {
    console::heading("rustlavel tinker");
    console::info(&console::dim(
        "Each line is compiled and run as its own program, so it costs a compile\n  \
         and nothing carries over: no variables, no connections, no history.\n  \
         It is a scratchpad, not a conversation.\n",
    ));
    console::info(&console::dim(
        "The first snippet builds the scratch crate in target/tinker and is slow;\n  \
         the ones after it reuse it. Every run prints what it cost.\n",
    ));
    console::info(&console::dim("  :q to leave, :help for the rest.\n"));
}

/// The scratch crate a snippet is compiled inside.
struct Scratch {
    /// Where the generated crate lives: `<project>/target/tinker`.
    directory: PathBuf,
    /// The application, so the snippet runs with its working directory.
    root: PathBuf,
    /// Shared with the application, so its dependencies are already built.
    target: PathBuf,
    package: String,
    crate_name: String,
    dependency: String,
}

impl Scratch {
    fn prepare(project: &Project) -> Result<Scratch, String> {
        let scratch = Scratch {
            directory: project.root.join("target/tinker"),
            root: project.root.clone(),
            target: project.root.join("target"),
            package: project.crate_name.clone(),
            crate_name: naming::snake(&project.crate_name),
            dependency: rustlavel_dependency(&project.root),
        };

        std::fs::create_dir_all(scratch.directory.join("src"))
            .map_err(|e| format!("cannot create target/tinker: {e}"))?;

        // Only written when it differs: rewriting it would make cargo
        // re-resolve the dependency graph on every snippet.
        let mut values = BTreeMap::new();
        values.insert("package", scratch.package.clone());
        values.insert("project", scratch.root.display().to_string());
        values.insert("dependency", scratch.dependency.clone());
        write_if_changed(
            &scratch.directory.join("Cargo.toml"),
            &render(stubs::TINKER_CARGO_TOML, &values),
        )?;

        Ok(scratch)
    }

    /// Compile and run one snippet. Returns whether it succeeded.
    fn evaluate(&self, snippet: &str) -> Result<bool, String> {
        let mut values = BTreeMap::new();
        values.insert("crate_name", self.crate_name.clone());
        values.insert("env_path", self.root.join(".env").display().to_string());
        values.insert("snippet", indent(&wrap_trailing_expression(snippet)));
        write_if_changed(
            &self.directory.join("src/main.rs"),
            &render(stubs::TINKER_MAIN_RS, &values),
        )?;

        let compile_started = Instant::now();
        let built = std::process::Command::new("cargo")
            .arg("build")
            .arg("--quiet")
            // The application's own `target/`, so everything it has already
            // built is reused and the first snippet only has to compile one
            // crate. Nothing else about the invocation may differ — setting
            // RUSTFLAGS here, for instance, would change every fingerprint and
            // rebuild the whole tree. The warnings a snippet would otherwise
            // produce are turned off inside the generated file instead.
            .env("CARGO_TARGET_DIR", &self.target)
            .current_dir(&self.directory)
            .status()
            .map_err(|e| format!("cannot run cargo: {e}"))?;
        let compiled_in = compile_started.elapsed();

        if !built.success() {
            console::info(&console::dim(&format!(
                "did not compile — {}",
                seconds(compiled_in)
            )));
            return Ok(false);
        }

        let binary = self.target.join("debug/tinker");
        let run_started = Instant::now();
        let status = std::process::Command::new(&binary)
            // The application's own directory, so `config/` and `.env` resolve
            // exactly as they do when it is serving.
            .current_dir(&self.root)
            .status()
            .map_err(|e| format!("cannot run {}: {e}", binary.display()))?;
        let ran_in = run_started.elapsed();

        console::info(&console::dim(&format!(
            "compiled in {} · ran in {}{}",
            seconds(compiled_in),
            seconds(ran_in),
            match status.code() {
                Some(0) | None => String::new(),
                Some(code) => format!(" · exit {code}"),
            }
        )));
        Ok(status.success())
    }
}

fn seconds(duration: Duration) -> String {
    format!("{:.2}s", duration.as_secs_f64())
}

/// Print the value of a trailing expression, the way a REPL would.
///
/// The rule is small enough to state: everything after the last `;` or `}` is
/// the tail, and if the tail is an expression rather than the start of a
/// statement it is wrapped in a `println!`. So `2 + 2` prints `4`, and
/// `let x = 2; x * 3` prints `6`, while `let x = 2;` prints nothing — which is
/// what it did.
fn wrap_trailing_expression(snippet: &str) -> String {
    let trimmed = snippet.trim_end();
    let split_at = trimmed.rfind([';', '}']).map_or(0, |at| at + 1);
    let (head, tail) = trimmed.split_at(split_at);

    let expression = tail.trim();
    if expression.is_empty() || starts_a_statement(expression) {
        return trimmed.to_string();
    }

    format!("{head}\n    println!(\"{{:#?}}\", {expression});")
}

/// Whether a fragment opens a statement rather than being a value.
fn starts_a_statement(fragment: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "let", "use", "fn", "pub", "struct", "enum", "impl", "trait", "mod", "const", "static",
        "type", "return", "break", "continue", "extern", "unsafe",
    ];

    if fragment.starts_with('#') || fragment.starts_with("//") {
        return true;
    }
    KEYWORDS.iter().any(|keyword| {
        fragment.strip_prefix(keyword).is_some_and(|rest| {
            rest.starts_with(|c: char| c.is_whitespace()) || rest.is_empty()
        })
    })
}

/// Four spaces, so the generated `main` reads like something a person wrote.
fn indent(snippet: &str) -> String {
    snippet
        .lines()
        .map(|line| if line.trim().is_empty() { String::new() } else { format!("    {line}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The application's own `rustlavel = ...` line, so the snippet compiles
/// against exactly the packages the application enabled.
///
/// A relative `path` is rewritten to an absolute one, because the scratch crate
/// sits two directories deeper than the manifest it was copied from.
fn rustlavel_dependency(root: &Path) -> String {
    let fallback = format!("\"{}\"", env!("CARGO_PKG_VERSION"));
    let Ok(manifest) = std::fs::read_to_string(root.join("Cargo.toml")) else { return fallback };

    let mut in_dependencies = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if !in_dependencies {
            continue;
        }
        let Some(rest) = line.strip_prefix("rustlavel") else { continue };
        let rest = rest.trim_start();
        let Some(value) = rest.strip_prefix('=') else { continue };
        return absolute_paths(value.trim(), root);
    }
    fallback
}

/// Rewrite `path = "../framework/..."` against the application's directory.
fn absolute_paths(spec: &str, root: &Path) -> String {
    let Some(open) = spec.find("path = \"") else { return spec.to_string() };
    let start = open + "path = \"".len();
    let Some(length) = spec[start..].find('"') else { return spec.to_string() };
    let path = &spec[start..start + length];

    if Path::new(path).is_absolute() {
        return spec.to_string();
    }
    let absolute = root.join(path);
    format!("{}{}{}", &spec[..start], absolute.display(), &spec[start + length..])
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), String> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailing_expression_is_printed() {
        assert_eq!(wrap_trailing_expression("2 + 2"), "\n    println!(\"{:#?}\", 2 + 2);");
        assert_eq!(
            wrap_trailing_expression("let x = 2; x * 3"),
            "let x = 2;\n    println!(\"{:#?}\", x * 3);"
        );
    }

    #[test]
    fn a_statement_is_left_alone() {
        for snippet in [
            "let x = 2;",
            "println!(\"hello\");",
            "let x = 2",
            "use std::fs;",
            "return Ok(())",
            "#[derive(Debug)] struct A;",
            "if true { 1 } else { 2 }",
        ] {
            assert_eq!(
                wrap_trailing_expression(snippet),
                snippet.trim_end(),
                "`{snippet}` should be inserted as it stands"
            );
        }
    }

    #[test]
    fn a_keyword_prefix_is_not_a_keyword() {
        // `lettuce` is not `let`.
        assert!(wrap_trailing_expression("lettuce").contains("println!"));
        assert!(!starts_a_statement("uses"));
        assert!(starts_a_statement("use std::fs;"));
    }

    #[test]
    fn an_empty_snippet_stays_empty() {
        assert_eq!(wrap_trailing_expression("   "), "");
        assert_eq!(wrap_trailing_expression("let x = 1;\n"), "let x = 1;");
    }

    #[test]
    fn the_snippet_is_indented_into_the_generated_main() {
        assert_eq!(indent("a();\nb();"), "    a();\n    b();");
        assert_eq!(indent("a();\n\nb();"), "    a();\n\n    b();");
    }

    #[test]
    fn a_relative_framework_path_is_made_absolute() {
        let root = Path::new("/apps/blog");
        let spec = "{ path = \"../framework/crates/rustlavel\", features = [\"db\"] }";

        assert_eq!(
            absolute_paths(spec, root),
            "{ path = \"/apps/blog/../framework/crates/rustlavel\", features = [\"db\"] }"
        );
    }

    #[test]
    fn an_absolute_path_and_a_version_are_left_alone() {
        let root = Path::new("/apps/blog");

        assert_eq!(absolute_paths("\"0.2.2\"", root), "\"0.2.2\"");
        assert_eq!(
            absolute_paths("{ path = \"/f/crates/rustlavel\" }", root),
            "{ path = \"/f/crates/rustlavel\" }"
        );
    }

    #[test]
    fn the_generated_main_has_no_placeholder_left() {
        let mut values = BTreeMap::new();
        values.insert("crate_name", "blog".to_string());
        values.insert("env_path", "/apps/blog/.env".to_string());
        values.insert("snippet", indent(&wrap_trailing_expression("1 + 1")));
        let main = render(stubs::TINKER_MAIN_RS, &values);

        assert!(!main.contains("{{"), "{main}");
        assert!(main.contains("use blog::*;"));
        assert!(main.contains("#[rustlavel::main]"));
        assert!(main.contains("println!"));

        let mut manifest_values = BTreeMap::new();
        manifest_values.insert("package", "blog".to_string());
        manifest_values.insert("project", "/apps/blog".to_string());
        manifest_values.insert("dependency", "{ version = \"0.2.2\" }".to_string());
        let manifest = render(stubs::TINKER_CARGO_TOML, &manifest_values);

        assert!(!manifest.contains("{{"), "{manifest}");
        // Its own workspace root, or cargo adopts it into whatever is above.
        assert!(manifest.contains("[workspace]"));
        assert!(manifest.contains("\"blog\" = { path = \"/apps/blog\" }"));
    }
}
