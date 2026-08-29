use std::fmt;

/// The framework-wide result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Every failure the framework itself can produce.
///
/// Application errors are expected to convert into this through `From`, which
/// is what lets a handler return `Result<T, E>` and still be a valid handler.
#[derive(Debug)]
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
