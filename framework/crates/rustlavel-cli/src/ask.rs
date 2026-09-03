//! Asking a person a question in a terminal.
//!
//! Written here rather than pulled in, for the same reason as everything else:
//! a prompt is a `println!`, a `read_line` and a loop, and a dependency for
//! that is a dependency to audit, pin and update forever.
//!
//! **The one rule that matters is that this must never block a script.** A
//! scaffold that stops for input in CI is a scaffold that hangs a build, so
//! every question checks [`interactive`] first and falls back to its default.
//! `--yes` turns the prompts off explicitly, and a non-terminal stdin turns
//! them off on its own.

use crate::console;
use std::io::{IsTerminal, Write};

/// Whether it makes sense to ask at all.
///
/// Both ends have to be a terminal. Piping the output into a file while stdin
/// is still a keyboard is a real thing people do, and a prompt whose question
/// went into the file is a prompt nobody can answer.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// One choice out of a list, by number.
///
/// Returns `default` when input is not available or the answer is empty, so
/// pressing Return is always the safe move.
pub fn choose(question: &str, options: &[(&str, &str)], default: usize) -> usize {
    if !interactive() || options.is_empty() {
        return default;
    }

    println!("\n{}", console::bold(question));
    for (index, (label, note)) in options.iter().enumerate() {
        let marker = if index == default { "›" } else { " " };
        println!("  {marker} {}. {label}", index + 1);
        if !note.is_empty() {
            println!("       {}", console::dim(note));
        }
    }

    loop {
        let answer = line(&format!(
            "  Choose 1–{} [{}]: ",
            options.len(),
            default + 1
        ));
        let answer = answer.trim();
        if answer.is_empty() {
            return default;
        }
        match answer.parse::<usize>() {
            Ok(n) if (1..=options.len()).contains(&n) => return n - 1,
            // Not an error and not a retry limit: somebody mistyping a number
            // should be able to try again, and a wrong number is not worth
            // ending the command over.
            _ => println!("  {}", console::dim("Give one of the numbers above.")),
        }
    }
}

/// Yes or no.
pub fn confirm(question: &str, default: bool) -> bool {
    if !interactive() {
        return default;
    }
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        let answer = line(&format!("{} [{hint}]: ", console::bold(question)));
        match answer.trim().to_ascii_lowercase().as_str() {
            "" => return default,
            "y" | "yes" => return true,
            "n" | "no" => return false,
            _ => println!("  {}", console::dim("Answer y or n.")),
        }
    }
}

/// Several choices out of a list, by number: `1,3,5`, or `all`, or nothing.
///
/// Returns the indices chosen, in the order they were listed rather than the
/// order they were typed, so the result does not depend on how somebody
/// happened to type it.
pub fn choose_many(question: &str, options: &[(&str, &str)], preselected: &[usize]) -> Vec<usize> {
    if !interactive() || options.is_empty() {
        return preselected.to_vec();
    }

    println!("\n{}", console::bold(question));
    for (index, (label, note)) in options.iter().enumerate() {
        let marker = if preselected.contains(&index) { "›" } else { " " };
        println!("  {marker} {:>2}. {label}", index + 1);
        if !note.is_empty() {
            println!("        {}", console::dim(note));
        }
    }
    println!(
        "  {}",
        console::dim("Numbers separated by commas, `all`, or nothing for none.")
    );

    loop {
        let answer = line("  Choose: ");
        let answer = answer.trim();

        if answer.is_empty() {
            return preselected.to_vec();
        }
        if answer.eq_ignore_ascii_case("all") {
            return (0..options.len()).collect();
        }
        if answer.eq_ignore_ascii_case("none") {
            return Vec::new();
        }

        let parsed: Result<Vec<usize>, ()> = answer
            .split([',', ' '])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| match part.parse::<usize>() {
                Ok(n) if (1..=options.len()).contains(&n) => Ok(n - 1),
                _ => Err(()),
            })
            .collect();

        match parsed {
            Ok(mut chosen) => {
                chosen.sort_unstable();
                chosen.dedup();
                return chosen;
            }
            Err(()) => println!(
                "  {}",
                console::dim(&format!("Use numbers from 1 to {}, separated by commas.", options.len()))
            ),
        }
    }
}

/// A line of free text, with a default.
pub fn text(question: &str, default: &str) -> String {
    if !interactive() {
        return default.to_string();
    }
    let shown = if default.is_empty() {
        format!("{}: ", console::bold(question))
    } else {
        format!("{} [{}]: ", console::bold(question), console::dim(default))
    };
    let answer = line(&shown);
    match answer.trim() {
        "" => default.to_string(),
        given => given.to_string(),
    }
}

/// Print a prompt and read one line.
///
/// End of input reads as empty rather than as an error: a person pressing
/// Ctrl-D means "stop asking me", and the caller's default is the right answer
/// to that.
fn line(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();

    let mut buffer = String::new();
    match std::io::stdin().read_line(&mut buffer) {
        Ok(0) | Err(_) => String::new(),
        Ok(_) => buffer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests run with stdin redirected, so `interactive()` is false and
    /// every question has to answer itself. That is the property that keeps
    /// this out of CI's way, so it is the one worth asserting.
    #[test]
    fn every_question_answers_itself_when_nobody_is_there() {
        assert!(!interactive(), "the test harness is not a terminal");

        assert_eq!(choose("Which?", &[("a", ""), ("b", "")], 1), 1);
        assert!(confirm("Really?", true));
        assert!(!confirm("Really?", false));
        assert_eq!(choose_many("Which?", &[("a", ""), ("b", "")], &[0]), vec![0]);
        assert_eq!(text("Name?", "app"), "app");
    }

    /// An empty list has no valid answer, so it must not prompt for one.
    #[test]
    fn an_empty_list_is_not_a_question() {
        assert_eq!(choose("Which?", &[], 0), 0);
        assert_eq!(choose_many("Which?", &[], &[]), Vec::<usize>::new());
    }
}
