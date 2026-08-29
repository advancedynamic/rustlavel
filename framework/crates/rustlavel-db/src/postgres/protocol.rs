//! PostgreSQL frontend/backend protocol, version 3.
//!
//! Messages are length-prefixed and big-endian. Frontend messages are built
//! into a [`Buffer`]; backend messages are parsed by [`Backend::parse`].

use rustlavel_core::{Error, Result};

/// The protocol version in the startup packet: major 3, minor 0.
pub const PROTOCOL_VERSION: i32 = 196_608;

/// A frontend message under construction.
#[derive(Default)]
pub struct Buffer {
    bytes: Vec<u8>,
}

impl Buffer {
    pub fn new() -> Self {
        Buffer::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn i16(&mut self, value: i16) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn i32(&mut self, value: i32) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_be_bytes());
        self
    }

    fn cstr(&mut self, value: &str) -> &mut Self {
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
        self
    }

    /// Write a message: a type byte, then a length that counts itself.
    fn message(&mut self, tag: u8, body: impl FnOnce(&mut Buffer)) -> &mut Self {
        self.bytes.push(tag);
        let length_at = self.bytes.len();
        self.bytes.extend_from_slice(&[0; 4]);

        body(self);

        let length = (self.bytes.len() - length_at) as i32;
        self.bytes[length_at..length_at + 4].copy_from_slice(&length.to_be_bytes());
        self
    }

    /// The startup packet, which has a length and a version but no type byte.
    pub fn startup(&mut self, parameters: &[(&str, &str)]) -> &mut Self {
        let length_at = self.bytes.len();
        self.bytes.extend_from_slice(&[0; 4]);
        self.i32(PROTOCOL_VERSION);

        for (key, value) in parameters {
            self.cstr(key);
            self.cstr(value);
        }
        self.bytes.push(0);

        let length = (self.bytes.len() - length_at) as i32;
        self.bytes[length_at..length_at + 4].copy_from_slice(&length.to_be_bytes());
        self
    }

    pub fn password(&mut self, password: &str) -> &mut Self {
        self.message(b'p', |buffer| {
            buffer.cstr(password);
        })
    }

    pub fn sasl_initial(&mut self, mechanism: &str, response: &str) -> &mut Self {
        self.message(b'p', |buffer| {
            buffer.cstr(mechanism);
            buffer.i32(response.len() as i32);
            buffer.bytes.extend_from_slice(response.as_bytes());
        })
    }

    pub fn sasl_response(&mut self, response: &str) -> &mut Self {
        self.message(b'p', |buffer| {
            buffer.bytes.extend_from_slice(response.as_bytes());
        })
    }

    /// A simple query. Multiple statements are allowed but no parameters, so
    /// this is reserved for DDL and internal bookkeeping.
    pub fn query(&mut self, sql: &str) -> &mut Self {
        self.message(b'Q', |buffer| {
            buffer.cstr(sql);
        })
    }

    /// Prepare an unnamed statement. Parameter types are left unspecified so
    /// the server infers them.
    pub fn parse(&mut self, name: &str, sql: &str) -> &mut Self {
        self.message(b'P', |buffer| {
            buffer.cstr(name);
            buffer.cstr(sql);
            buffer.i16(0);
        })
    }

    /// Bind parameters, sent in text format.
    ///
    /// Text keeps the driver free of per-type binary encoders while remaining
    /// exact: the server parses each parameter with the type it inferred.
    pub fn bind(&mut self, portal: &str, statement: &str, params: &[Option<String>]) -> &mut Self {
        self.message(b'B', |buffer| {
            buffer.cstr(portal);
            buffer.cstr(statement);
            // No format codes: everything is text.
            buffer.i16(0);
            buffer.i16(params.len() as i16);
            for param in params {
                match param {
                    None => {
                        buffer.i32(-1);
                    }
                    Some(text) => {
                        buffer.i32(text.len() as i32);
                        buffer.bytes.extend_from_slice(text.as_bytes());
                    }
                }
            }
            // Results in text format too.
            buffer.i16(0);
        })
    }

    pub fn describe_portal(&mut self, portal: &str) -> &mut Self {
        self.message(b'D', |buffer| {
            buffer.bytes.push(b'P');
            buffer.cstr(portal);
        })
    }

    pub fn execute(&mut self, portal: &str, max_rows: i32) -> &mut Self {
        self.message(b'E', |buffer| {
            buffer.cstr(portal);
            buffer.i32(max_rows);
        })
    }

    pub fn sync(&mut self) -> &mut Self {
        self.message(b'S', |_| {})
    }

    pub fn terminate(&mut self) -> &mut Self {
        self.message(b'X', |_| {})
    }
}

/// One column's metadata from a `RowDescription`.
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub type_oid: i32,
}

/// An error or notice sent by the server.
#[derive(Debug, Clone, Default)]
pub struct ServerError {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// Byte offset into the statement, when the server could point at one.
    pub position: Option<usize>,
}

impl ServerError {
    pub fn into_error(self, sql: Option<&str>) -> Error {
        let mut text = format!("{}: {}", self.code, self.message);
        if let Some(detail) = &self.detail {
            text.push_str(&format!(" — {detail}"));
        }
        if let Some(hint) = &self.hint {
            text.push_str(&format!(" (hint: {hint})"));
        }
        // Pointing at the offending statement is the difference between a
        // usable error and a puzzle.
        if let Some(sql) = sql {
            text.push_str(&format!("\n  SQL: {sql}"));
        }
        Error::msg(text)
    }
}

/// A message received from the server.
#[derive(Debug)]
pub enum Backend {
    Authentication(Authentication),
    ParameterStatus { name: String, value: String },
    BackendKeyData { process_id: i32, secret: i32 },
    RowDescription(Vec<Field>),
    DataRow(Vec<Option<Vec<u8>>>),
    CommandComplete(String),
    EmptyQueryResponse,
    ReadyForQuery(TransactionStatus),
    Error(ServerError),
    Notice(ServerError),
    ParseComplete,
    BindComplete,
    CloseComplete,
    NoData,
    PortalSuspended,
    NotificationResponse { channel: String, payload: String },
    /// Anything the driver does not need to act on.
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStatus {
    Idle,
    InTransaction,
    Failed,
}

#[derive(Debug)]
pub enum Authentication {
    Ok,
    CleartextPassword,
    Md5Password { salt: [u8; 4] },
    Sasl { mechanisms: Vec<String> },
    SaslContinue { data: String },
    SaslFinal { data: String },
    /// A mechanism this driver does not implement (GSS, SSPI).
    Unsupported(i32),
}

impl Backend {
    /// Parse one message body, given its type byte.
    pub fn parse(tag: u8, body: &[u8]) -> Result<Backend> {
        let mut reader = Reader::new(body);

        Ok(match tag {
            b'R' => Backend::Authentication(parse_authentication(&mut reader)?),
            b'S' => Backend::ParameterStatus {
                name: reader.cstr()?,
                value: reader.cstr()?,
            },
            b'K' => Backend::BackendKeyData {
                process_id: reader.i32()?,
                secret: reader.i32()?,
            },
            b'T' => {
                let count = reader.i16()?;
                let mut fields = Vec::with_capacity(count.max(0) as usize);
                for _ in 0..count {
                    let name = reader.cstr()?;
                    reader.skip(6)?; // table oid, column index
                    let type_oid = reader.i32()?;
                    reader.skip(8)?; // type size, type modifier, format code
                    fields.push(Field { name, type_oid });
                }
                Backend::RowDescription(fields)
            }
            b'D' => {
                let count = reader.i16()?;
                let mut values = Vec::with_capacity(count.max(0) as usize);
                for _ in 0..count {
                    let length = reader.i32()?;
                    values.push(if length < 0 {
                        None
                    } else {
                        Some(reader.take(length as usize)?.to_vec())
                    });
                }
                Backend::DataRow(values)
            }
            b'C' => Backend::CommandComplete(reader.cstr()?),
            b'I' => Backend::EmptyQueryResponse,
            b'Z' => Backend::ReadyForQuery(match reader.u8()? {
                b'T' => TransactionStatus::InTransaction,
                b'E' => TransactionStatus::Failed,
                _ => TransactionStatus::Idle,
            }),
            b'E' => Backend::Error(parse_server_error(&mut reader)?),
            b'N' => Backend::Notice(parse_server_error(&mut reader)?),
            b'A' => {
                reader.i32()?;
                Backend::NotificationResponse {
                    channel: reader.cstr()?,
                    payload: reader.cstr()?,
                }
            }
            b'1' => Backend::ParseComplete,
            b'2' => Backend::BindComplete,
            b'3' => Backend::CloseComplete,
            b'n' => Backend::NoData,
            b's' => Backend::PortalSuspended,
            other => Backend::Other(other),
        })
    }
}

fn parse_authentication(reader: &mut Reader<'_>) -> Result<Authentication> {
    Ok(match reader.i32()? {
        0 => Authentication::Ok,
        3 => Authentication::CleartextPassword,
        5 => {
            let mut salt = [0u8; 4];
            salt.copy_from_slice(reader.take(4)?);
            Authentication::Md5Password { salt }
        }
        10 => {
            let mut mechanisms = Vec::new();
            loop {
                let mechanism = reader.cstr()?;
                if mechanism.is_empty() {
                    break;
                }
                mechanisms.push(mechanism);
            }
            Authentication::Sasl { mechanisms }
        }
        11 => Authentication::SaslContinue { data: reader.rest_string() },
        12 => Authentication::SaslFinal { data: reader.rest_string() },
        other => Authentication::Unsupported(other),
    })
}

fn parse_server_error(reader: &mut Reader<'_>) -> Result<ServerError> {
    let mut error = ServerError::default();

    loop {
        let field = reader.u8()?;
        if field == 0 {
            break;
        }
        let value = reader.cstr()?;
        match field {
            b'S' => error.severity = value,
            b'C' => error.code = value,
            b'M' => error.message = value,
            b'D' => error.detail = Some(value),
            b'H' => error.hint = Some(value),
            b'P' => error.position = value.parse().ok(),
            _ => {}
        }
    }

    Ok(error)
}

/// A cursor over a message body.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, position: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.position + count;
        if end > self.bytes.len() {
            return Err(Error::Protocol("truncated message from the server".into()));
        }
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    fn skip(&mut self, count: usize) -> Result<()> {
        self.take(count).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn cstr(&mut self) -> Result<String> {
        let start = self.position;
        while self.position < self.bytes.len() && self.bytes[self.position] != 0 {
            self.position += 1;
        }
        if self.position >= self.bytes.len() {
            return Err(Error::Protocol("unterminated string from the server".into()));
        }
        let text = String::from_utf8_lossy(&self.bytes[start..self.position]).into_owned();
        self.position += 1;
        Ok(text)
    }

    fn rest_string(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.bytes[self.position..]).into_owned();
        self.position = self.bytes.len();
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_startup_packet() {
        let bytes = Buffer::new()
            .startup(&[("user", "ada"), ("database", "blog")])
            .clone_bytes();

        let length = i32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
        assert_eq!(length, bytes.len());
        assert_eq!(i32::from_be_bytes(bytes[4..8].try_into().unwrap()), PROTOCOL_VERSION);
        assert_eq!(*bytes.last().unwrap(), 0);
    }

    #[test]
    fn a_frontend_message_length_counts_itself() {
        let bytes = Buffer::new().query("select 1").clone_bytes();

        assert_eq!(bytes[0], b'Q');
        let length = i32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
        assert_eq!(length, bytes.len() - 1);
        assert_eq!(&bytes[5..], b"select 1\0");
    }

    #[test]
    fn binds_null_parameters_as_minus_one() {
        let bytes = Buffer::new()
            .bind("", "", &[Some("7".to_string()), None])
            .clone_bytes();

        // Two parameters, the second encoded with length -1.
        assert!(bytes.windows(4).any(|w| w == (-1i32).to_be_bytes()));
    }

    #[test]
    fn parses_a_row_description() {
        let mut body = Vec::new();
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(b"id\0");
        body.extend_from_slice(&0i32.to_be_bytes()); // table oid
        body.extend_from_slice(&0i16.to_be_bytes()); // column index
        body.extend_from_slice(&23i32.to_be_bytes()); // int4
        body.extend_from_slice(&4i16.to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());

        match Backend::parse(b'T', &body).unwrap() {
            Backend::RowDescription(fields) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].name, "id");
                assert_eq!(fields[0].type_oid, 23);
            }
            other => panic!("expected a row description, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_data_row_with_a_null() {
        let mut body = Vec::new();
        body.extend_from_slice(&2i16.to_be_bytes());
        body.extend_from_slice(&3i32.to_be_bytes());
        body.extend_from_slice(b"ada");
        body.extend_from_slice(&(-1i32).to_be_bytes());

        match Backend::parse(b'D', &body).unwrap() {
            Backend::DataRow(values) => {
                assert_eq!(values[0].as_deref(), Some(&b"ada"[..]));
                assert_eq!(values[1], None);
            }
            other => panic!("expected a data row, got {other:?}"),
        }
    }

    #[test]
    fn parses_an_error_response() {
        let mut body = Vec::new();
        body.push(b'S');
        body.extend_from_slice(b"ERROR\0");
        body.push(b'C');
        body.extend_from_slice(b"42P01\0");
        body.push(b'M');
        body.extend_from_slice(b"relation \"nope\" does not exist\0");
        body.push(0);

        match Backend::parse(b'E', &body).unwrap() {
            Backend::Error(error) => {
                assert_eq!(error.code, "42P01");
                assert!(error.message.contains("does not exist"));
                let rendered = error.into_error(Some("select * from nope")).to_string();
                assert!(rendered.contains("SQL: select * from nope"));
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_message_is_a_protocol_error() {
        let error = Backend::parse(b'K', &[0, 0]).unwrap_err();
        assert!(error.to_string().contains("truncated"));
    }

    impl Buffer {
        fn clone_bytes(&self) -> Vec<u8> {
            self.bytes.clone()
        }
    }
}
