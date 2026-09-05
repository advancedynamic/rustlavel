//! Splitting template source into literal text, echoes, and directives.
//!
//! The lexer knows the *shape* of the syntax but nothing about its meaning:
//! `@endif` is just a directive here, and whether it has a matching `@if` is
//! the parser's problem.

use crate::source::syntax_error;
use rustlavel_core::Result;

/// Directives that take a parenthesised argument list.
const WITH_ARGUMENTS: [&str; 9] =
    ["if", "elseif", "foreach", "extends", "section", "yield", "include", "lang", "route"];

/// Directives that stand alone.
const WITHOUT_ARGUMENTS: [&str; 4] = ["else", "endif", "endforeach", "endsection"];

/// Directives that structure a template rather than emit anything.
///
/// When one of these sits alone on a line, the line disappears with it —
/// otherwise every `@if` would leave a blank line in the response body and the
/// rendered HTML would be unreadable in a browser's view-source.
const STRUCTURAL: [&str; 9] = [
    "if",
    "elseif",
    "else",
    "endif",
    "foreach",
    "endforeach",
    "section",
    "endsection",
    "extends",
];

/// One piece of a template, still unparsed.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Literal markup, copied to the output verbatim.
    Text(String),
    /// `{{ expr }}` (escaped) or `{!! expr !!}` (raw).
    Echo { source: String, offset: usize, escaped: bool },
    /// `@name` plus the raw text between its parentheses, empty when it had none.
    Directive { name: String, arguments: String, arguments_offset: usize, offset: usize },
}

/// Split `source` into tokens, dropping `{{-- comments --}}` entirely.
pub fn tokenize(file: &str, source: &str) -> Result<Vec<Token>> {
    let mut lexer = Lexer { file, source, pos: 0, text: String::new(), tokens: Vec::new() };
    lexer.run()?;
    Ok(lexer.tokens)
}

struct Lexer<'a> {
    file: &'a str,
    source: &'a str,
    pos: usize,
    /// Literal text accumulated since the last token was emitted.
    text: String,
    tokens: Vec<Token>,
}

impl Lexer<'_> {
    fn run(&mut self) -> Result<()> {
        // Copied out so the slices below borrow the source, not `self`.
        let source = self.source;

        while self.pos < source.len() {
            let rest = &source[self.pos..];

            // Nothing interesting can start except at a `{` or an `@`, so skip
            // straight to the next one and copy everything before it.
            match rest.find(['{', '@']) {
                None => {
                    self.text.push_str(rest);
                    self.pos = source.len();
                    continue;
                }
                Some(0) => {}
                Some(index) => {
                    self.text.push_str(&rest[..index]);
                    self.pos += index;
                    continue;
                }
            }

            let rest = &source[self.pos..];
            if rest.starts_with("{{--") {
                let end = self.find_close("--}}", self.pos + 4, false, "{{--")?;
                self.pos = end + 4;
            } else if rest.starts_with("{!!") {
                self.echo(3, "!!}", false)?;
            } else if rest.starts_with("{{") {
                self.echo(2, "}}", true)?;
            } else if rest.starts_with("@@") {
                // `@@if` is how a template writes a literal `@if`.
                self.text.push('@');
                self.pos += 2;
            } else if rest.starts_with("@{{") {
                // `@{{ x }}` hands the braces to a client-side framework.
                self.text.push_str("{{");
                self.pos += 3;
            } else if rest.starts_with('@') && self.directive()? {
                continue;
            } else {
                // A lone `{`, or an `@` that starts no known directive — an
                // email address or a CSS `@media` block. Text, not syntax.
                self.text.push(source.as_bytes()[self.pos] as char);
                self.pos += 1;
            }
        }

        self.flush();
        Ok(())
    }

    /// Emit the pending literal text, if any.
    fn flush(&mut self) {
        if !self.text.is_empty() {
            self.tokens.push(Token::Text(std::mem::take(&mut self.text)));
        }
    }

    fn echo(&mut self, open: usize, close: &str, escaped: bool) -> Result<()> {
        let source = self.source;
        let start = self.pos + open;
        let end = self.find_close(close, start, true, &source[self.pos..start])?;
        self.flush();
        self.tokens.push(Token::Echo {
            source: source[start..end].to_string(),
            offset: start,
            escaped,
        });
        self.pos = end + close.len();
        Ok(())
    }

    /// Try to read a directive at the current `@`.
    ///
    /// Returns `false` when the name is not one we know, leaving the caller to
    /// treat the `@` as text.
    fn directive(&mut self) -> Result<bool> {
        let source = self.source;
        let start = self.pos;
        let bytes = source.as_bytes();
        let mut end = start + 1;
        while matches!(bytes.get(end), Some(byte) if byte.is_ascii_alphabetic()) {
            end += 1;
        }
        let name = &source[start + 1..end];

        // `@php` is not supported and never will be: a template that can run
        // code stops being reviewable as markup, and the compiler cannot see
        // inside it. Say so instead of silently rendering `@php` as text.
        if name == "php" || name == "endphp" {
            return Err(syntax_error(
                self.file,
                self.source,
                start,
                "`@php` is not supported — compute the value in Rust and pass it to the view",
            ));
        }

        let takes_arguments = WITH_ARGUMENTS.contains(&name);
        if !takes_arguments && !WITHOUT_ARGUMENTS.contains(&name) {
            return Ok(false);
        }

        let (arguments, arguments_offset, mut after) = if takes_arguments {
            if bytes.get(end) != Some(&b'(') {
                return Err(syntax_error(
                    self.file,
                    self.source,
                    end,
                    format!("expected `(` after `@{name}`"),
                ));
            }
            let close = self.matching_paren(end)?;
            (source[end + 1..close].to_string(), end + 1, close + 1)
        } else {
            (String::new(), end, end)
        };

        // Swallow the line when the directive owns it by itself.
        if STRUCTURAL.contains(&name)
            && self.at_line_start(start)
            && let Some(next_line) = self.line_end(after)
        {
            self.text.truncate(self.text.trim_end_matches([' ', '\t']).len());
            after = next_line;
        }

        let name = name.to_string();
        self.flush();
        self.tokens.push(Token::Directive { name, arguments, arguments_offset, offset: start });
        self.pos = after;
        Ok(true)
    }

    /// Find `close` at or after `from`, optionally stepping over string
    /// literals so that `{{ "}}" }}` is not cut in half.
    fn find_close(
        &self,
        close: &str,
        from: usize,
        quoted: bool,
        opener: &str,
    ) -> Result<usize> {
        let bytes = self.source.as_bytes();
        let mut i = from;
        while i < bytes.len() {
            if quoted && matches!(bytes[i], b'"' | b'\'') {
                i = self.skip_string(i);
                continue;
            }
            if bytes[i..].starts_with(close.as_bytes()) {
                return Ok(i);
            }
            i += 1;
        }
        Err(syntax_error(
            self.file,
            self.source,
            from - opener.len(),
            format!("`{opener}` is never closed — add a matching `{close}`"),
        ))
    }

    /// The offset just past a string literal starting at `start`.
    fn skip_string(&self, start: usize) -> usize {
        let bytes = self.source.as_bytes();
        let quote = bytes[start];
        let mut i = start + 1;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                byte if byte == quote => return i + 1,
                _ => i += 1,
            }
        }
        i
    }

    /// The offset of the `)` matching the `(` at `open`.
    fn matching_paren(&self, open: usize) -> Result<usize> {
        let bytes = self.source.as_bytes();
        let mut depth = 0usize;
        let mut i = open;
        while i < bytes.len() {
            match bytes[i] {
                b'"' | b'\'' => {
                    i = self.skip_string(i);
                    continue;
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        Err(syntax_error(self.file, self.source, open, "`(` is never closed"))
    }

    /// True when only spaces and tabs sit between `offset` and its line start.
    fn at_line_start(&self, offset: usize) -> bool {
        self.source[..offset]
            .bytes()
            .rev()
            .find(|byte| !matches!(byte, b' ' | b'\t'))
            .is_none_or(|byte| byte == b'\n')
    }

    /// The offset of the next line, when only spaces and tabs follow `offset`.
    fn line_end(&self, offset: usize) -> Option<usize> {
        let bytes = self.source.as_bytes();
        let mut i = offset;
        while matches!(bytes.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        match bytes.get(i) {
            Some(b'\n') => Some(i + 1),
            Some(b'\r') if bytes.get(i + 1) == Some(&b'\n') => Some(i + 2),
            None => Some(i),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustlavel_core::Error;

    fn tokens(source: &str) -> Vec<Token> {
        tokenize("test", source).unwrap()
    }

    fn text_of(source: &str) -> String {
        tokens(source)
            .into_iter()
            .filter_map(|token| match token {
                Token::Text(text) => Some(text),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn separates_text_from_echoes() {
        assert_eq!(
            tokens("<h1>{{ title }}</h1>"),
            vec![
                Token::Text("<h1>".into()),
                Token::Echo { source: " title ".into(), offset: 6, escaped: true },
                Token::Text("</h1>".into()),
            ]
        );
    }

    #[test]
    fn a_raw_echo_is_marked_unescaped() {
        assert_eq!(
            tokens("{!! body !!}"),
            vec![Token::Echo { source: " body ".into(), offset: 3, escaped: false }]
        );
    }

    #[test]
    fn comments_are_stripped_entirely() {
        assert_eq!(text_of("a{{-- secret {{ x }} --}}b"), "ab");
    }

    #[test]
    fn reads_a_directive_with_its_arguments() {
        assert_eq!(
            tokens("@if(a == \")\")x@endif"),
            vec![
                Token::Directive {
                    name: "if".into(),
                    arguments: "a == \")\"".into(),
                    arguments_offset: 4,
                    offset: 0,
                },
                Token::Text("x".into()),
                Token::Directive {
                    name: "endif".into(),
                    arguments: String::new(),
                    arguments_offset: 20,
                    offset: 14,
                },
            ]
        );
    }

    #[test]
    fn an_unknown_at_sign_stays_text() {
        assert_eq!(text_of("@media print { a { color: #000 } } ada@example.com"),
                   "@media print { a { color: #000 } } ada@example.com");
        assert_eq!(text_of("@@if(x)"), "@if(x)");
        assert_eq!(text_of("@{{ vue }}"), "{{ vue }}");
    }

    #[test]
    fn a_structural_directive_alone_on_a_line_takes_the_line_with_it() {
        let source = "<ul>\n  @foreach(items as item)\n    <li></li>\n  @endforeach\n</ul>\n";

        assert_eq!(text_of(source), "<ul>\n    <li></li>\n</ul>\n");
    }

    #[test]
    fn an_inline_directive_keeps_the_surrounding_spacing() {
        assert_eq!(text_of("a @if(x)b@endif c"), "a b c");
    }

    #[test]
    fn php_blocks_are_rejected_by_name() {
        let error = tokenize("test", "<p>\n@php $x = 1; @endphp\n").unwrap_err();

        match error {
            Error::Template { line, message, .. } => {
                assert_eq!(line, 2);
                assert!(message.contains("@php"), "{message}");
            }
            other => panic!("expected a template error, got {other:?}"),
        }
    }

    #[test]
    fn an_unclosed_echo_is_reported_where_it_opened() {
        match tokenize("test", "ok\n{{ title\n").unwrap_err() {
            Error::Template { line, column, .. } => assert_eq!((line, column), (2, 1)),
            other => panic!("expected a template error, got {other:?}"),
        }
    }
}
