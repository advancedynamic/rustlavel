use std::fmt;

/// The framework-wide result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every failure the framework itself can produce.
///
/// Application errors are expected to convert into this through `From`, which
/// is what lets a handler return `Result<T, E>` and still be a valid handler.
pub enum Error {
    /// A file could not be read or written.
    Io(std::io::Error),
    /// A `.env` or config file was malformed.
    Config { file: String, line: usize, message: String },
    /// A JSON document could not be parsed.
    Json { line: usize, column: usize, message: String },
    /// A template could not be parsed or rendered.
    ///
    /// Carries the position because a view is written by hand, often by
    /// someone who is not the person reading the stack trace.
    Template { file: String, line: usize, column: usize, message: String },
    /// A malformed HTTP request reached the parser.
    Protocol(String),
    /// A dependency was not tried, because it is known to be failing.
    ///
    /// Distinct from a failure on purpose: nothing was sent, so nothing can
    /// have had an effect, and a caller may safely fall back to a cache, a
    /// default, or a degraded answer. Raised by the circuit breaker in
    /// `rustlavel-client`.
    Unavailable(String),
    /// Anything raised by application code.
    Message(String),
}

impl Error {
    pub fn msg(message: impl Into<String>) -> Self {
        Error::Message(message.into())
    }

    /// The HTTP status this error should surface as when it escapes a handler.
    pub fn status(&self) -> u16 {
        match self {
            Error::Protocol(_) => 400,
            // The upstream this request needed is known to be down, and the
            // caller may reasonably try again later — which is what 503 means.
            Error::Unavailable(_) => 503,
            _ => 500,
        }
    }

    /// A short, human title for the dev error page.
    pub fn title(&self) -> &'static str {
        match self {
            Error::Io(_) => "I/O Error",
            Error::Config { .. } => "Configuration Error",
            Error::Json { .. } => "JSON Error",
            Error::Template { .. } => "Template Error",
            Error::Protocol(_) => "Protocol Error",
            Error::Unavailable(_) => "Dependency Unavailable",
            Error::Message(_) => "Application Error",
        }
    }

    /// A suggestion shown on the dev error page — the Ignition touch.
    pub fn hint(&self) -> Option<String> {
        match self {
            Error::Config { file, .. } => Some(format!(
                "Check the syntax of `{file}`. Each line should look like `KEY=value`."
            )),
            Error::Json { .. } => {
                Some("Verify the payload is valid JSON — trailing commas are not allowed.".into())
            }
            Error::Template { file, line, .. } => Some(format!(
                "Look at `{file}` around line {line}. Every `@if`, `@foreach` and `@section` \
                 needs its matching `@end...`."
            )),
            Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Some("The file does not exist. Did you run `rustlavel new` in this directory?".into())
            }
            Error::Io(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                Some("That port is already in use. Try `rustlavel serve --port 8001`.".into())
            }
            _ => None,
        }
    }
}

/// Deliberately the same as [`Display`](fmt::Display).
///
/// Rust prints the error a `main` returns with `Debug`, not `Display`, so the
/// derived form is what people actually see when an application fails to
/// start. It reads like this:
///
/// ```text
/// Error: Io(Os { code: 48, kind: AddrInUse, message: "Address already in use" })
/// ```
///
/// which names the struct that holds the problem rather than the problem, and
/// tells somebody nothing they can act on. Delegating to `Display` gives them
/// the sentence that was written for them instead. Test output improves for
/// the same reason: `unwrap_err()` on a bad config now says which file and
/// line, not which variant.
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Config { file, line, message } => {
                write!(f, "{file}:{line}: {message}")
            }
            Error::Json { line, column, message } => {
                write!(f, "invalid JSON at line {line}, column {column}: {message}")
            }
            Error::Template { file, line, column, message } => {
                write!(f, "{file}:{line}:{column}: {message}")
            }
            Error::Protocol(m) => write!(f, "malformed request: {m}"),
            Error::Unavailable(m) => f.write_str(m),
            Error::Message(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error::Message(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error::Message(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure this exists to stop: `Error: Io(Os { code: 48, kind:
    /// AddrInUse, ... })` at the top of somebody's terminal, naming the struct
    /// that holds the problem instead of the problem.
    #[test]
    fn the_debug_form_is_the_sentence_not_the_struct() {
        let error = Error::Io(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "Address already in use",
        ));
        let shown = format!("{error:?}");
        assert_eq!(shown, format!("{error}"));
        assert!(!shown.contains("Io("), "the variant is leaking: {shown}");
        assert!(!shown.contains("Os {"), "the os struct is leaking: {shown}");
        assert!(shown.contains("Address already in use"), "{shown}");
    }

    #[test]
    fn a_config_error_debugs_to_its_file_and_line() {
        let error = Error::Config {
            file: ".env".into(),
            line: 4,
            message: "expected KEY=value".into(),
        };
        assert_eq!(format!("{error:?}"), ".env:4: expected KEY=value");
    }
}
