//! Terminal output helpers — the look of every rustlavel command.

use std::io::IsTerminal;

fn styled(code: &str, text: &str) -> String {
    if std::io::stdout().is_terminal() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn dim(text: &str) -> String {
    styled("38;5;244", text)
}

pub fn bold(text: &str) -> String {
    styled("1", text)
}

pub fn accent(text: &str) -> String {
    styled("38;5;173", text)
}

/// A file that was created.
pub fn created(path: &str) {
    println!("  {} {path}", styled("38;5;71", "created"));
}

pub fn updated(path: &str) {
    println!("  {} {path}", styled("38;5;179", "updated"));
}

pub fn info(message: &str) {
    println!("  {message}");
}

pub fn success(message: &str) {
    println!("\n{} {message}\n", styled("38;5;71", "✓"));
}

pub fn error(message: &str) {
    eprintln!("\n{} {message}\n", styled("38;5;203", "✗"));
}

pub fn heading(text: &str) {
    println!("\n{}", bold(text));
}
