//! The small expression language inside `{{ }}` and `@if(...)`.
//!
//! It is deliberately tiny: paths, literals, comparisons, and boolean logic.
//! There is no arithmetic, no method calls, and no assignment — a view that
//! needs any of those is doing work that belongs in Rust, where the compiler
//! can check it and a test can cover it.

use crate::context::Scope;
use crate::source::Span;
use crate::value::{compare, truthy};
use rustlavel_core::{Json, Result};
use std::cmp::Ordering;

/// A parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A dotted lookup into the render data: `user.name`, `items.0.title`.
    Path(String),
    Str(String),
    Number(f64),
    Bool(bool),
    Null,
    Not(Box<Expr>),
    Binary { op: BinaryOp, left: Box<Expr>, right: Box<Expr> },
}

/// The operators, loosest binding first: `||`, `&&`, equality, comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Expr {
    /// Evaluate against the current scope.
    ///
    /// A path that resolves to nothing yields `Json::Null` rather than an
    /// error. Rendering is the last step of a request: a typo in a view should
    /// leave a hole in the page, not turn a working page into a 500.
    pub fn eval(&self, scope: &Scope) -> Json {
        match self {
            Expr::Path(path) => scope.lookup(path).cloned().unwrap_or(Json::Null),
            Expr::Str(text) => Json::String(text.clone()),
            Expr::Number(number) => Json::Number(*number),
            Expr::Bool(flag) => Json::Bool(*flag),
            Expr::Null => Json::Null,
            Expr::Not(inner) => Json::Bool(!truthy(&inner.eval(scope))),
            Expr::Binary { op, left, right } => match op {
                // Short-circuiting matters: `@if(user && user.admin)` must not
                // walk into a null.
                BinaryOp::And => {
                    Json::Bool(truthy(&left.eval(scope)) && truthy(&right.eval(scope)))
                }
                BinaryOp::Or => {
                    Json::Bool(truthy(&left.eval(scope)) || truthy(&right.eval(scope)))
                }
                _ => {
                    let (left, right) = (left.eval(scope), right.eval(scope));
                    Json::Bool(match op {
                        // Equality is strict: `"3" == 3` is false, because a
                        // coercion table is one more thing to memorise.
                        BinaryOp::Eq => left == right,
                        BinaryOp::Ne => left != right,
                        BinaryOp::Lt => compare(&left, &right) == Some(Ordering::Less),
                        BinaryOp::Le => {
                            matches!(compare(&left, &right), Some(Ordering::Less | Ordering::Equal))
                        }
                        BinaryOp::Gt => compare(&left, &right) == Some(Ordering::Greater),
                        _ => matches!(
                            compare(&left, &right),
                            Some(Ordering::Greater | Ordering::Equal)
                        ),
                    })
                }
            },
        }
    }
}

/// Parse exactly one expression, rejecting anything left over.
pub fn parse(span: Span<'_>) -> Result<Expr> {
    let mut cursor = Cursor::new(span)?;
    let expr = cursor.expression()?;
    cursor.expect_end()?;
    Ok(expr)
}

/// Parse a comma-separated argument list, as `@section("title", "Home")` and
/// `@yield("body", "nothing here yet")` need.
pub fn parse_arguments(span: Span<'_>) -> Result<Vec<Expr>> {
    let mut cursor = Cursor::new(span)?;
    let mut arguments = Vec::new();
    if cursor.peek().is_some() {
        loop {
            arguments.push(cursor.expression()?);
            if cursor.peek() == Some(&Token::Comma) {
                cursor.pos += 1;
                continue;
            }
            break;
        }
    }
    cursor.expect_end()?;
    Ok(arguments)
}

/// Parse the head of `@foreach(items as item)` into its subject and binding.
pub fn parse_foreach(span: Span<'_>) -> Result<(Expr, String)> {
    let mut cursor = Cursor::new(span)?;
    let subject = cursor.expression()?;

    // `as` is not an operator, so the expression parser stops in front of it.
    match cursor.peek() {
        Some(Token::Path(word)) if word == "as" => cursor.pos += 1,
        _ => {
            return Err(cursor
                .error_here("expected `as`, like `@foreach(users as user)`"));
        }
    }

    let binding = match cursor.peek().cloned() {
        Some(Token::Path(name)) if !name.contains('.') => {
            cursor.pos += 1;
            name
        }
        _ => return Err(cursor.error_here("expected a variable name after `as`")),
    };

    cursor.expect_end()?;
    Ok((subject, binding))
}

/// One lexical unit of an expression.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Path(String),
    Str(String),
    Number(f64),
    True,
    False,
    Null,
    Not,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Open,
    Close,
    Comma,
}

impl Token {
    /// How the token is named in an error message.
    fn describe(&self) -> String {
        match self {
            Token::Path(name) => format!("`{name}`"),
            Token::Str(_) => "a string".to_string(),
            Token::Number(_) => "a number".to_string(),
            Token::True => "`true`".to_string(),
            Token::False => "`false`".to_string(),
            Token::Null => "`null`".to_string(),
            Token::Not => "`!`".to_string(),
            Token::And => "`&&`".to_string(),
            Token::Or => "`||`".to_string(),
            Token::Eq => "`==`".to_string(),
            Token::Ne => "`!=`".to_string(),
            Token::Lt => "`<`".to_string(),
            Token::Le => "`<=`".to_string(),
            Token::Gt => "`>`".to_string(),
            Token::Ge => "`>=`".to_string(),
            Token::Open => "`(`".to_string(),
            Token::Close => "`)`".to_string(),
            Token::Comma => "`,`".to_string(),
        }
    }
}

/// Recursive-descent parser over a tokenised expression.
struct Cursor<'a> {
    span: Span<'a>,
    tokens: Vec<(Token, usize)>,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(span: Span<'a>) -> Result<Self> {
        Ok(Cursor { tokens: tokenize(span)?, span, pos: 0 })
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|(token, _)| token)
    }

    /// The offset of the current token, or the end of the fragment.
    fn offset(&self) -> usize {
        self.tokens.get(self.pos).map_or(self.span.text.len(), |(_, offset)| *offset)
    }

    fn error_here(&self, message: impl Into<String>) -> rustlavel_core::Error {
        self.span.error(self.offset(), message)
    }

    /// Consume `token` if it is next.
    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn expect_end(&self) -> Result<()> {
        match self.peek() {
            None => Ok(()),
            Some(token) => Err(self.error_here(format!("unexpected {}", token.describe()))),
        }
    }

    /// The precedence ladder, loosest first: `||`, `&&`, equality,
    /// comparison, `!`, primary. Four rungs is the whole language.
    fn expression(&mut self) -> Result<Expr> {
        self.any_of(&[(Token::Or, BinaryOp::Or)], Cursor::conjunction)
    }

    fn conjunction(&mut self) -> Result<Expr> {
        self.any_of(&[(Token::And, BinaryOp::And)], Cursor::equality)
    }

    fn equality(&mut self) -> Result<Expr> {
        self.any_of(&[(Token::Eq, BinaryOp::Eq), (Token::Ne, BinaryOp::Ne)], Cursor::comparison)
    }

    fn comparison(&mut self) -> Result<Expr> {
        // Two-character operators are listed first only for readability; the
        // tokenizer already decided which one it saw.
        self.any_of(
            &[
                (Token::Le, BinaryOp::Le),
                (Token::Ge, BinaryOp::Ge),
                (Token::Lt, BinaryOp::Lt),
                (Token::Gt, BinaryOp::Gt),
            ],
            Cursor::unary,
        )
    }

    /// Left-associative chain of `operators` over the next tighter rung.
    fn any_of(
        &mut self,
        operators: &[(Token, BinaryOp)],
        tighter: fn(&mut Self) -> Result<Expr>,
    ) -> Result<Expr> {
        let mut left = tighter(self)?;
        'chain: loop {
            for (token, op) in operators {
                if self.eat(token) {
                    let right = tighter(self)?;
                    left = Expr::Binary { op: *op, left: Box::new(left), right: Box::new(right) };
                    continue 'chain;
                }
            }
            return Ok(left);
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat(&Token::Not) {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr> {
        let Some(token) = self.peek().cloned() else {
            return Err(self.error_here("unexpected end of expression"));
        };
        self.pos += 1;

        match token {
            Token::Open => {
                let inner = self.expression()?;
                if !self.eat(&Token::Close) {
                    return Err(self.error_here("expected `)`"));
                }
                Ok(inner)
            }
            Token::Path(name) => Ok(Expr::Path(name)),
            Token::Str(text) => Ok(Expr::Str(text)),
            Token::Number(number) => Ok(Expr::Number(number)),
            Token::True => Ok(Expr::Bool(true)),
            Token::False => Ok(Expr::Bool(false)),
            Token::Null => Ok(Expr::Null),
            other => {
                self.pos -= 1;
                Err(self.error_here(format!("unexpected {}", other.describe())))
            }
        }
    }
}

/// Split an expression fragment into tokens, remembering each one's offset.
fn tokenize(span: Span<'_>) -> Result<Vec<(Token, usize)>> {
    let text = span.text;
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let start = i;
        let token = match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            b'"' | b'\'' => {
                let (value, next) = string_literal(span, i)?;
                i = next;
                Token::Str(value)
            }
            b'0'..=b'9' => {
                let (value, next) = number_literal(span, i)?;
                i = next;
                Token::Number(value)
            }
            // A `-` is only ever the sign of a literal; there is no arithmetic.
            b'-' if matches!(bytes.get(i + 1), Some(b'0'..=b'9')) => {
                let (value, next) = number_literal(span, i)?;
                i = next;
                Token::Number(value)
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                while matches!(
                    bytes.get(i),
                    Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.')
                ) {
                    i += 1;
                }
                let word = &text[start..i];
                if word.ends_with('.') || word.contains("..") {
                    return Err(span.error(start, "a path segment is missing after `.`"));
                }
                match word {
                    "true" => Token::True,
                    "false" => Token::False,
                    "null" | "nil" => Token::Null,
                    _ => Token::Path(word.to_string()),
                }
            }
            b'(' => {
                i += 1;
                Token::Open
            }
            b')' => {
                i += 1;
                Token::Close
            }
            b',' => {
                i += 1;
                Token::Comma
            }
            b'&' | b'|' => {
                let word = if bytes[i] == b'&' { "&&" } else { "||" };
                if !bytes[i..].starts_with(word.as_bytes()) {
                    return Err(span.error(start, format!("expected `{word}`")));
                }
                i += 2;
                if word == "&&" { Token::And } else { Token::Or }
            }
            b'=' | b'!' | b'<' | b'>' => {
                let two = bytes.get(i + 1) == Some(&b'=');
                i += if two { 2 } else { 1 };
                match (bytes[start], two) {
                    (b'=', true) => Token::Eq,
                    (b'=', false) => {
                        return Err(span.error(start, "use `==` to compare; there is no assignment in a template"));
                    }
                    (b'!', true) => Token::Ne,
                    (b'!', false) => Token::Not,
                    (b'<', true) => Token::Le,
                    (b'<', false) => Token::Lt,
                    (b'>', true) => Token::Ge,
                    _ => Token::Gt,
                }
            }
            byte => {
                return Err(span.error(start, format!("unexpected character `{}`", byte as char)));
            }
        };
        tokens.push((token, start));
    }

    Ok(tokens)
}

/// Read a quoted string, honouring the escapes an HTML author actually types.
fn string_literal(span: Span<'_>, start: usize) -> Result<(String, usize)> {
    let bytes = span.text.as_bytes();
    let quote = bytes[start];
    let mut value = String::new();
    let mut i = start + 1;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let escape = *bytes
                    .get(i + 1)
                    .ok_or_else(|| span.error(start, "unterminated string"))?;
                value.push(match escape {
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    other => other as char,
                });
                i += 2;
            }
            byte if byte == quote => {
                return Ok((value, i + 1));
            }
            _ => {
                // Copy whole UTF-8 sequences: the delimiters are all ASCII, so
                // anything else belongs to the string as-is.
                let from = i;
                while i < bytes.len() && bytes[i] != quote && bytes[i] != b'\\' {
                    i += 1;
                }
                value.push_str(&span.text[from..i]);
            }
        }
    }

    Err(span.error(start, "unterminated string"))
}

fn number_literal(span: Span<'_>, start: usize) -> Result<(f64, usize)> {
    let bytes = span.text.as_bytes();
    let mut i = start + usize::from(bytes[start] == b'-');
    while matches!(bytes.get(i), Some(b'0'..=b'9' | b'.')) {
        i += 1;
    }
    span.text[start..i]
        .parse::<f64>()
        .map(|number| (number, i))
        .map_err(|_| span.error(start, "invalid number"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::Context;
    use rustlavel_core::Error;

    fn expr(text: &str) -> Expr {
        parse(Span { file: "test", source: text, offset: 0, text }).unwrap()
    }

    fn eval(text: &str, context: &Context) -> Json {
        expr(text).eval(&Scope::new(context))
    }

    #[test]
    fn parses_paths_and_literals() {
        assert_eq!(expr("user.name"), Expr::Path("user.name".to_string()));
        assert_eq!(expr("'hi there'"), Expr::Str("hi there".to_string()));
        assert_eq!(expr(r#""hi""#), Expr::Str("hi".to_string()));
        assert_eq!(expr("42"), Expr::Number(42.0));
        assert_eq!(expr("-1.5"), Expr::Number(-1.5));
        assert_eq!(expr("true"), Expr::Bool(true));
        assert_eq!(expr("null"), Expr::Null);
    }

    #[test]
    fn and_binds_tighter_than_or() {
        let context = Context::new();

        // Parsed as `a || (b && c)`: with `a` false and `b` false the whole
        // thing is false, which only holds if `&&` won.
        assert_eq!(eval("false || false && true", &context), Json::Bool(false));
        assert_eq!(eval("(false || false) && true", &context), Json::Bool(false));
        assert_eq!(eval("true || false && false", &context), Json::Bool(true));
    }

    #[test]
    fn comparisons_bind_tighter_than_boolean_operators() {
        let context = Context::new().with("age", 30);

        assert_eq!(eval("age > 18 && age < 65", &context), Json::Bool(true));
        assert_eq!(eval("age == 30", &context), Json::Bool(true));
        assert_eq!(eval("age != 30", &context), Json::Bool(false));
        assert_eq!(eval("age >= 30 && age <= 30", &context), Json::Bool(true));
    }

    #[test]
    fn negation_applies_to_truthiness() {
        let context = Context::new().with("name", "");

        assert_eq!(eval("!name", &context), Json::Bool(true));
        assert_eq!(eval("!missing", &context), Json::Bool(true));
        assert_eq!(eval("!!name", &context), Json::Bool(false));
    }

    #[test]
    fn an_unknown_path_evaluates_to_null_instead_of_failing() {
        assert_eq!(eval("user.address.city", &Context::new()), Json::Null);
    }

    #[test]
    fn parses_an_argument_list() {
        let text = "\"title\", 'Home'";
        let span = Span { file: "test", source: text, offset: 0, text };
        let arguments = parse_arguments(span).unwrap();

        assert_eq!(arguments, vec![Expr::Str("title".into()), Expr::Str("Home".into())]);
    }

    #[test]
    fn parses_a_foreach_head() {
        let text = "user.posts as post";
        let span = Span { file: "test", source: text, offset: 0, text };
        let (subject, binding) = parse_foreach(span).unwrap();

        assert_eq!(subject, Expr::Path("user.posts".into()));
        assert_eq!(binding, "post");
    }

    #[test]
    fn reports_the_column_of_a_broken_expression() {
        let source = "\n@if(a == )\n";
        let span = Span::new("home", source, 5, 5);

        match parse(span).unwrap_err() {
            Error::Template { line, column, message, .. } => {
                assert_eq!((line, column), (2, 10));
                assert!(message.contains("unexpected end"), "{message}");
            }
            other => panic!("expected a template error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_assignment_and_stray_operators() {
        let cases = ["a = 1", "a & b", "a +"];
        for case in cases {
            let span = Span { file: "test", source: case, offset: 0, text: case };
            assert!(parse(span).is_err(), "`{case}` should not parse");
        }
    }
}
