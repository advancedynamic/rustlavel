//! Turning a byte offset back into the line and column an author can act on.
//!
//! Positions are only reconstructed when an error is raised, so the lexer and
//! the parsers stay free of per-character bookkeeping — the same trade the JSON
//! parser in core makes.

use rustlavel_core::Error;

/// The 1-based line and column of `offset` inside `source`.
///
/// Columns are counted in bytes, which matches the rest of the framework and
/// only differs from characters on lines that already contain non-ASCII text.
pub fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let consumed = &source.as_bytes()[..offset];
    let line = consumed.iter().filter(|byte| **byte == b'\n').count() + 1;
    let column =
        consumed.iter().rposition(|byte| *byte == b'\n').map_or(offset, |i| offset - i - 1) + 1;
    (line, column)
}

/// Build a template error pointing at `offset` inside `source`.
pub fn syntax_error(
    file: &str,
    source: &str,
    offset: usize,
    message: impl Into<String>,
) -> Error {
    let (line, column) = line_column(source, offset);
    Error::Template { file: file.to_string(), line, column, message: message.into() }
}

/// A fragment of a template that remembers where it was cut from.
///
/// Expressions are parsed out of `@if(...)` and `{{ ... }}` long after the
/// lexer has moved on; carrying the origin along is what lets an error inside
/// one still name the file, line, and column the author typed it at.
#[derive(Debug, Clone, Copy)]
pub struct Span<'a> {
    /// The view name, used as the file in error messages.
    pub file: &'a str,
    /// The full template source, needed to count lines.
    pub source: &'a str,
    /// Where `text` starts inside `source`.
    pub offset: usize,
    /// The fragment itself.
    pub text: &'a str,
}

impl<'a> Span<'a> {
    pub fn new(file: &'a str, source: &'a str, offset: usize, len: usize) -> Self {
        Span { file, source, offset, text: &source[offset..offset + len] }
    }

    /// Report an error at `at`, an offset *within* this fragment.
    pub fn error(&self, at: usize, message: impl Into<String>) -> Error {
        syntax_error(self.file, self.source, self.offset + at, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_lines_and_columns_from_one() {
        let source = "hello\n  world\n";

        assert_eq!(line_column(source, 0), (1, 1));
        assert_eq!(line_column(source, 4), (1, 5));
        assert_eq!(line_column(source, 6), (2, 1));
        assert_eq!(line_column(source, 8), (2, 3));
    }

    #[test]
    fn an_offset_past_the_end_lands_on_the_last_position() {
        assert_eq!(line_column("ab", 99), (1, 3));
    }

    #[test]
    fn a_span_reports_positions_relative_to_its_fragment() {
        let source = "line one\n@if(x == )\n";
        let span = Span::new("home", source, 13, 5);

        assert_eq!(span.text, "x == ");
        match span.error(5, "unexpected end of expression") {
            Error::Template { file, line, column, .. } => {
                assert_eq!((file.as_str(), line, column), ("home", 2, 10));
            }
            other => panic!("expected a template error, got {other:?}"),
        }
    }
}
