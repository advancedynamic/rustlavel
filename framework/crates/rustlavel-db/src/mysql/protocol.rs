//! The MySQL client/server protocol, written directly on the wire format.
//!
//! Every exchange is framed the same way: a 3-byte little-endian payload
//! length, a 1-byte sequence id, then the payload. The sequence id restarts at
//! zero for each command and must increase by one for every packet either side
//! sends, which is how both ends notice a lost or reordered packet.
//!
//! Everything here is little-endian — the opposite of PostgreSQL — and lengths
//! are usually *length-encoded integers*, a variable-width form that spends one
//! byte on the small numbers that dominate a result set.
//!
//! Client packets are built into a [`Buffer`]; server packets are parsed by the
//! `parse_*` functions and by [`Packet::parse`].

use crate::mysql::types;
use crate::value::Value;
use rustlavel_core::{Error, Result};

/// The largest payload one frame can carry: 2^24 - 1.
///
/// A longer payload is split across frames, and the split is only detectable by
/// a frame that is exactly this long, so a payload of exactly 16 MiB is
/// followed by an empty frame.
pub const MAX_PAYLOAD: usize = 0xFF_FF_FF;

/// The largest payload we tell the server we can receive.
pub const MAX_PACKET_SIZE: u32 = 1 << 24;

/// `utf8mb4_general_ci` — the framework speaks UTF-8, and `utf8mb3` cannot hold
/// an emoji, which is the kind of bug that only shows up in production.
pub const CHARSET_UTF8MB4: u8 = 45;

/// The `binary` collation id. A column carrying it holds bytes, not text.
pub const CHARSET_BINARY: u16 = 63;

// --- Capability flags ---
//
// Negotiated in the handshake: the server advertises what it supports, the
// client answers with the subset it wants, and the intersection governs the
// shape of every packet afterwards.

pub const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
pub const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
pub const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
pub const CLIENT_LOCAL_FILES: u32 = 0x0000_0080;
pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
pub const CLIENT_SSL: u32 = 0x0000_0800;
pub const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
pub const CLIENT_MULTI_STATEMENTS: u32 = 0x0001_0000;
pub const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;
pub const CLIENT_PS_MULTI_RESULTS: u32 = 0x0004_0000;
pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
pub const CLIENT_CONNECT_ATTRS: u32 = 0x0010_0000;
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
pub const CLIENT_SESSION_TRACK: u32 = 0x0080_0000;
pub const CLIENT_DEPRECATE_EOF: u32 = 0x0100_0000;

/// What this driver asks for.
///
/// `CLIENT_MULTI_STATEMENTS` is deliberately absent: with it the server would
/// accept `a; b` in one `COM_QUERY`, which turns any missed escape anywhere in
/// the stack into a second statement. Leaving it off means the server itself
/// refuses, rather than the framework having to.
///
/// `CLIENT_LOCAL_FILES` is absent for the same reason — it lets the *server*
/// ask the client to upload a local file, and a compromised or hostile server
/// should not be able to read the application's disk.
pub const CLIENT_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_LONG_FLAG
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_RESULTS
    | CLIENT_PS_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH
    | CLIENT_CONNECT_ATTRS
    | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
    | CLIENT_SESSION_TRACK;

// --- Server status flags ---

/// Set while a transaction is open — the only way to know without tracking
/// `begin`/`commit` ourselves, and therefore the only way that stays right when
/// a statement implicitly commits.
pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
pub const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

// --- Column flags ---

pub const NOT_NULL_FLAG: u16 = 0x0001;
pub const UNSIGNED_FLAG: u16 = 0x0020;
pub const BINARY_FLAG: u16 = 0x0080;

// --- Commands ---

pub const COM_QUIT: u8 = 0x01;
pub const COM_QUERY: u8 = 0x03;
pub const COM_PING: u8 = 0x0E;
pub const COM_STMT_PREPARE: u8 = 0x16;
pub const COM_STMT_EXECUTE: u8 = 0x17;
pub const COM_STMT_CLOSE: u8 = 0x19;

/// `COM_STMT_EXECUTE` without a server-side cursor: every row comes back at
/// once, which is what the driver's `Vec<Row>` result wants anyway.
pub const CURSOR_TYPE_NO_CURSOR: u8 = 0x00;

/// A client packet payload under construction.
///
/// Holds only the payload; framing is added by [`frame`] when the connection
/// knows which sequence id the packet must carry.
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

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.bytes.push(value);
        self
    }

    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn raw(&mut self, value: &[u8]) -> &mut Self {
        self.bytes.extend_from_slice(value);
        self
    }

    /// A NUL-terminated string.
    pub fn cstr(&mut self, value: &str) -> &mut Self {
        // A NUL inside the value would terminate the field early and shift
        // every field after it, so it is dropped rather than trusted.
        self.bytes.extend(value.bytes().filter(|byte| *byte != 0));
        self.bytes.push(0);
        self
    }

    /// A length-encoded integer: one byte for the small values that dominate a
    /// result set, and a marker byte plus 2, 3 or 8 bytes for the rest.
    pub fn lenenc_int(&mut self, value: u64) -> &mut Self {
        match value {
            // 0xFB and 0xFF are reserved as markers, so the one-byte form stops
            // just below them.
            0..=0xFA => self.bytes.push(value as u8),
            0xFB..=0xFFFF => {
                self.bytes.push(0xFC);
                self.bytes.extend_from_slice(&(value as u16).to_le_bytes());
            }
            0x1_0000..=0xFF_FFFF => {
                self.bytes.push(0xFD);
                self.bytes.extend_from_slice(&(value as u32).to_le_bytes()[..3]);
            }
            _ => {
                self.bytes.push(0xFE);
                self.bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        self
    }

    /// A length-encoded byte string.
    pub fn lenenc_bytes(&mut self, value: &[u8]) -> &mut Self {
        self.lenenc_int(value.len() as u64);
        self.bytes.extend_from_slice(value);
        self
    }

    /// The reply to the server's handshake: capabilities, then credentials.
    pub fn handshake_response(
        &mut self,
        capabilities: u32,
        user: &str,
        auth_response: &[u8],
        database: Option<&str>,
        plugin: &str,
        attributes: &[(&str, &str)],
    ) -> &mut Self {
        self.u32(capabilities);
        self.u32(MAX_PACKET_SIZE);
        self.u8(CHARSET_UTF8MB4);
        self.raw(&[0u8; 23]);
        self.cstr(user);

        // Length-encoded rather than a single length byte, so an auth response
        // longer than 255 bytes (the full caching_sha2 exchange) still fits.
        if capabilities & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            self.lenenc_bytes(auth_response);
        } else {
            self.u8(auth_response.len() as u8);
            self.raw(auth_response);
        }

        if capabilities & CLIENT_CONNECT_WITH_DB != 0 {
            self.cstr(database.unwrap_or(""));
        }
        if capabilities & CLIENT_PLUGIN_AUTH != 0 {
            self.cstr(plugin);
        }
        if capabilities & CLIENT_CONNECT_ATTRS != 0 {
            let mut pairs = Buffer::new();
            for (key, value) in attributes {
                pairs.lenenc_bytes(key.as_bytes());
                pairs.lenenc_bytes(value.as_bytes());
            }
            let pairs = pairs.into_bytes();
            self.lenenc_int(pairs.len() as u64);
            self.raw(&pairs);
        }
        self
    }

    /// The reply to an `AuthSwitchRequest`, or the extra data a plugin needs.
    ///
    /// It is a bare payload with no command byte: the server already knows what
    /// it asked for.
    pub fn auth_response(&mut self, data: &[u8]) -> &mut Self {
        self.raw(data)
    }

    /// A statement executed as text. No parameters, so it is reserved for DDL
    /// and transaction control.
    pub fn com_query(&mut self, sql: &str) -> &mut Self {
        self.u8(COM_QUERY);
        self.raw(sql.as_bytes())
    }

    /// Ask the server whether it is still there. Used to check a pooled
    /// connection without the cost of a round trip through the parser.
    pub fn com_ping(&mut self) -> &mut Self {
        self.u8(COM_PING)
    }

    pub fn com_quit(&mut self) -> &mut Self {
        self.u8(COM_QUIT)
    }

    /// Ask the server to parse a statement and hand back a handle for it.
    pub fn com_stmt_prepare(&mut self, sql: &str) -> &mut Self {
        self.u8(COM_STMT_PREPARE);
        self.raw(sql.as_bytes())
    }

    /// Run a prepared statement with values bound out of band.
    ///
    /// The values travel as typed binary in their own section of the packet and
    /// are never spliced into the statement text, which is what makes injection
    /// structurally impossible rather than a matter of remembering to escape.
    pub fn com_stmt_execute(&mut self, statement_id: u32, params: &[Value]) -> &mut Self {
        self.u8(COM_STMT_EXECUTE);
        self.u32(statement_id);
        self.u8(CURSOR_TYPE_NO_CURSOR);
        // Iteration count is always 1; the protocol reserves the field but the
        // server rejects any other value.
        self.u32(1);

        if params.is_empty() {
            return self;
        }

        // One bit per parameter, least significant bit first, saying which are
        // NULL. A NULL contributes a bit and nothing else — it has no value
        // section at all.
        let mut null_bitmap = vec![0u8; params.len().div_ceil(8)];
        for (index, param) in params.iter().enumerate() {
            if param.is_null() {
                null_bitmap[index / 8] |= 1 << (index % 8);
            }
        }
        self.raw(&null_bitmap);

        // "New parameters bound": the types that follow are authoritative. The
        // driver never reuses a statement handle across differently typed
        // arguments, so this is always 1.
        self.u8(1);
        for param in params {
            let (column_type, unsigned) = types::bind_type(param);
            self.u8(column_type);
            self.u8(if unsigned { 0x80 } else { 0x00 });
        }
        for param in params {
            types::encode_bind(param, &mut self.bytes);
        }
        self
    }

    /// Release a prepared statement's server-side resources.
    ///
    /// The server sends nothing back, so a connection that skips this leaks a
    /// handle for the life of the session.
    pub fn com_stmt_close(&mut self, statement_id: u32) -> &mut Self {
        self.u8(COM_STMT_CLOSE);
        self.u32(statement_id)
    }
}

/// Wrap a payload in one or more frames, starting at `sequence`.
///
/// A payload of `MAX_PAYLOAD` or more is split, and a payload that is an exact
/// multiple of `MAX_PAYLOAD` gets a trailing empty frame — without it the
/// server would wait forever for a continuation that never comes.
pub fn frame(payload: &[u8], sequence: u8) -> (Vec<u8>, u8) {
    let mut out = Vec::with_capacity(payload.len() + 4);
    let mut sequence = sequence;
    let mut rest = payload;

    loop {
        let take = rest.len().min(MAX_PAYLOAD);
        out.extend_from_slice(&(take as u32).to_le_bytes()[..3]);
        out.push(sequence);
        out.extend_from_slice(&rest[..take]);
        sequence = sequence.wrapping_add(1);
        rest = &rest[take..];

        if take < MAX_PAYLOAD {
            break;
        }
    }

    (out, sequence)
}

/// One column's metadata, from a `ColumnDefinition41` packet.
#[derive(Debug, Clone, Default)]
pub struct Column {
    /// The name as the result set presents it, which is the alias when there is
    /// one — that is what `row.get("total")` has to match.
    pub name: String,
    pub original_name: String,
    pub table: String,
    pub charset: u16,
    pub length: u32,
    pub column_type: u8,
    pub flags: u16,
    pub decimals: u8,
}

impl Column {
    /// Whether the column's values are bytes rather than text.
    ///
    /// MySQL distinguishes `blob` from `text` only by collation, so a `blob`
    /// and a `text` column arrive with the same type byte and are told apart
    /// here.
    pub fn is_binary(&self) -> bool {
        self.charset == CHARSET_BINARY
    }

    pub fn is_unsigned(&self) -> bool {
        self.flags & UNSIGNED_FLAG != 0
    }
}

/// The server's opening `Handshake` packet, protocol version 10.
#[derive(Debug, Clone, Default)]
pub struct Handshake {
    pub server_version: String,
    pub connection_id: u32,
    /// The 20-byte nonce every password plugin salts its digest with.
    pub scramble: Vec<u8>,
    pub capabilities: u32,
    pub charset: u8,
    pub status: u16,
    pub auth_plugin: String,
}

/// An `OK` packet: what the server sends when a statement produced no rows.
#[derive(Debug, Clone, Default)]
pub struct OkPacket {
    pub affected_rows: u64,
    /// The key an `auto_increment` column generated, or 0 when none did.
    ///
    /// This is the whole reason `QueryResult::last_insert_id` exists: MySQL
    /// reports the key here rather than as a row.
    pub last_insert_id: u64,
    pub status: u16,
    pub warnings: u16,
    pub info: String,
}

/// An `EOF` packet, marking the end of a section of a result set.
#[derive(Debug, Clone, Default)]
pub struct EofPacket {
    pub warnings: u16,
    pub status: u16,
}

/// An error the server reported, carrying the code people search for.
#[derive(Debug, Clone, Default)]
pub struct ServerError {
    pub code: u16,
    /// The five-character SQLSTATE, when the server sent one. Absent only from
    /// pre-4.1 servers and from a handshake that failed before negotiation.
    pub sql_state: String,
    pub message: String,
}

impl ServerError {
    /// Render for a human, naming the code, the SQL state and the statement.
    ///
    /// The code is the part that is searchable and the statement is the part
    /// that says where to look, so both are always present when known.
    pub fn into_error(self, sql: Option<&str>) -> Error {
        let mut text = if self.sql_state.is_empty() {
            format!("MySQL error {}: {}", self.code, self.message)
        } else {
            format!("MySQL error {} ({}): {}", self.code, self.sql_state, self.message)
        };

        if let Some(sql) = sql {
            text.push_str(&format!("\n  SQL: {sql}"));
        }
        Error::msg(text)
    }
}

/// The server's answer to `COM_STMT_PREPARE`.
#[derive(Debug, Clone, Default)]
pub struct PrepareOk {
    pub statement_id: u32,
    pub columns: u16,
    pub params: u16,
    pub warnings: u16,
}

/// A packet whose kind can be told from its first byte.
///
/// Only unambiguous in reply to a command. Inside a result set a leading `0x00`
/// is a binary row and a leading `0xFE` is a length-encoded integer, so rows
/// are parsed by the code that knows it asked for them.
#[derive(Debug)]
pub enum Packet {
    Ok(OkPacket),
    Err(ServerError),
    Eof(EofPacket),
    /// The server wants a different authentication plugin than the one the
    /// handshake named.
    AuthSwitch { plugin: String, data: Vec<u8> },
    /// Plugin-specific data mid-authentication, such as caching_sha2's verdict.
    AuthMoreData(Vec<u8>),
    /// A payload the caller has to interpret itself — a result-set header.
    Other(Vec<u8>),
}

impl Packet {
    /// Classify one payload received in reply to a command.
    pub fn parse(payload: &[u8]) -> Result<Packet> {
        match payload.first() {
            None => Err(Error::Protocol("the server sent an empty packet".into())),
            Some(0x00) => Ok(Packet::Ok(parse_ok(payload)?)),
            Some(0xFF) => Ok(Packet::Err(parse_err(payload)?)),
            Some(0x01) => Ok(Packet::AuthMoreData(payload[1..].to_vec())),
            // `0xFE` is EOF only in a short packet; in a longer one it is the
            // marker of an 8-byte length-encoded integer, or an auth switch.
            Some(0xFE) if payload.len() < 9 => Ok(Packet::Eof(parse_eof(payload)?)),
            Some(0xFE) => {
                let mut reader = Reader::new(&payload[1..]);
                Ok(Packet::AuthSwitch {
                    plugin: reader.cstr()?,
                    data: trim_trailing_nul(reader.rest()).to_vec(),
                })
            }
            Some(_) => Ok(Packet::Other(payload.to_vec())),
        }
    }
}

/// Whether a payload is an `ERR` packet. Checked before anything else, because
/// an error can arrive in place of any expected packet.
pub fn is_err(payload: &[u8]) -> bool {
    payload.first() == Some(&0xFF)
}

/// Whether a payload is an `EOF` packet rather than a row.
pub fn is_eof(payload: &[u8]) -> bool {
    payload.first() == Some(&0xFE) && payload.len() < 9
}

/// Parse the server's opening handshake.
pub fn parse_handshake(payload: &[u8]) -> Result<Handshake> {
    let mut reader = Reader::new(payload);

    let version = reader.u8()?;
    if version != 10 {
        return Err(Error::Protocol(format!(
            "the server speaks handshake protocol {version}; this driver implements version 10. \
             MySQL 4.0 and older are not supported."
        )));
    }

    let mut handshake = Handshake {
        server_version: reader.cstr()?,
        connection_id: reader.u32()?,
        ..Handshake::default()
    };

    // The nonce arrives in two pieces with unrelated bytes between them, a
    // legacy of the field having been extended in place.
    handshake.scramble.extend_from_slice(reader.take(8)?);
    reader.skip(1)?; // filler

    let lower = reader.u16()? as u32;
    handshake.capabilities = lower;

    // A minimal server stops here; everything after is optional.
    if reader.is_empty() {
        return Ok(handshake);
    }

    handshake.charset = reader.u8()?;
    handshake.status = reader.u16()?;
    handshake.capabilities |= (reader.u16()? as u32) << 16;

    let scramble_length = reader.u8()?;
    reader.skip(10)?; // reserved

    if handshake.capabilities & CLIENT_SECURE_CONNECTION != 0 {
        // The field is at least 13 bytes whatever the plugin says it needs, and
        // the last of those is a NUL that is not part of the nonce.
        let rest = (scramble_length as usize).saturating_sub(8).max(13);
        let part = reader.take(rest.min(reader.remaining()))?;
        handshake.scramble.extend_from_slice(trim_trailing_nul(part));
    }

    if handshake.capabilities & CLIENT_PLUGIN_AUTH != 0 && !reader.is_empty() {
        handshake.auth_plugin = reader.cstr().unwrap_or_default();
    }

    Ok(handshake)
}

/// Parse an `OK` packet.
pub fn parse_ok(payload: &[u8]) -> Result<OkPacket> {
    let mut reader = Reader::new(payload);
    reader.skip(1)?; // the 0x00 header

    Ok(OkPacket {
        affected_rows: reader.lenenc_int()?,
        last_insert_id: reader.lenenc_int()?,
        status: reader.u16().unwrap_or(0),
        warnings: reader.u16().unwrap_or(0),
        info: String::from_utf8_lossy(reader.rest()).into_owned(),
    })
}

/// Parse an `ERR` packet, including its SQL state.
pub fn parse_err(payload: &[u8]) -> Result<ServerError> {
    let mut reader = Reader::new(payload);
    reader.skip(1)?; // the 0xFF header

    let code = reader.u16()?;

    // A `#` marks the SQLSTATE field. Its absence means a pre-4.1 server, or an
    // error raised before capabilities were negotiated.
    let sql_state = if reader.peek() == Some(b'#') {
        reader.skip(1)?;
        String::from_utf8_lossy(reader.take(5)?).into_owned()
    } else {
        String::new()
    };

    Ok(ServerError {
        code,
        sql_state,
        message: String::from_utf8_lossy(reader.rest()).into_owned(),
    })
}

/// Parse an `EOF` packet.
pub fn parse_eof(payload: &[u8]) -> Result<EofPacket> {
    let mut reader = Reader::new(payload);
    reader.skip(1)?; // the 0xFE header

    Ok(EofPacket {
        warnings: reader.u16().unwrap_or(0),
        status: reader.u16().unwrap_or(0),
    })
}

/// Parse a `ColumnDefinition41` packet.
pub fn parse_column(payload: &[u8]) -> Result<Column> {
    let mut reader = Reader::new(payload);

    reader.lenenc_bytes()?; // catalog, always "def"
    reader.lenenc_bytes()?; // schema
    let table = String::from_utf8_lossy(reader.lenenc_bytes()?).into_owned();
    reader.lenenc_bytes()?; // original table
    let name = String::from_utf8_lossy(reader.lenenc_bytes()?).into_owned();
    let original_name = String::from_utf8_lossy(reader.lenenc_bytes()?).into_owned();

    reader.lenenc_int()?; // length of the fixed-length section that follows

    Ok(Column {
        name,
        original_name,
        table,
        charset: reader.u16()?,
        length: reader.u32()?,
        column_type: reader.u8()?,
        flags: reader.u16()?,
        decimals: reader.u8()?,
    })
}

/// Parse the response to `COM_STMT_PREPARE`.
pub fn parse_prepare_ok(payload: &[u8]) -> Result<PrepareOk> {
    let mut reader = Reader::new(payload);
    reader.skip(1)?; // the 0x00 status byte

    let statement_id = reader.u32()?;
    let columns = reader.u16()?;
    let params = reader.u16()?;
    reader.skip(1).ok(); // reserved filler

    Ok(PrepareOk {
        statement_id,
        columns,
        params,
        warnings: reader.u16().unwrap_or(0),
    })
}

/// Split a text-protocol row into its columns.
///
/// Every value is a length-encoded string, and `0xFB` in place of a length is
/// how NULL is spelled — which is why an empty string and a NULL are still
/// distinguishable.
pub fn parse_text_row(payload: &[u8], columns: usize) -> Result<Vec<Option<Vec<u8>>>> {
    let mut reader = Reader::new(payload);
    let mut values = Vec::with_capacity(columns);

    for _ in 0..columns {
        values.push(match reader.lenenc_bytes_or_null()? {
            Some(bytes) => Some(bytes.to_vec()),
            None => None,
        });
    }

    Ok(values)
}

/// Drop a single trailing NUL, which several fields carry as a terminator that
/// is not part of the value.
fn trim_trailing_nul(bytes: &[u8]) -> &[u8] {
    match bytes.last() {
        Some(0) => &bytes[..bytes.len() - 1],
        _ => bytes,
    }
}

/// A cursor over a packet payload.
pub struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, position: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.position >= self.bytes.len()
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    pub fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    pub fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(count).ok_or_else(truncated)?;
        if end > self.bytes.len() {
            return Err(truncated());
        }
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    pub fn skip(&mut self, count: usize) -> Result<()> {
        self.take(count).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    /// A length-encoded integer. `0xFB` (NULL) is an error here; the callers
    /// that allow it use [`Reader::lenenc_bytes_or_null`].
    pub fn lenenc_int(&mut self) -> Result<u64> {
        match self.u8()? {
            marker @ 0..=0xFA => Ok(marker as u64),
            0xFB => Err(Error::Protocol("a NULL where a length was expected".into())),
            0xFC => Ok(self.u16()? as u64),
            0xFD => {
                let bytes = self.take(3)?;
                Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) as u64)
            }
            _ => self.u64(),
        }
    }

    pub fn lenenc_bytes(&mut self) -> Result<&'a [u8]> {
        let length = self.lenenc_int()? as usize;
        self.take(length)
    }

    pub fn lenenc_bytes_or_null(&mut self) -> Result<Option<&'a [u8]>> {
        if self.peek() == Some(0xFB) {
            self.position += 1;
            return Ok(None);
        }
        self.lenenc_bytes().map(Some)
    }

    pub fn cstr(&mut self) -> Result<String> {
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

    pub fn rest(&mut self) -> &'a [u8] {
        let slice = &self.bytes[self.position.min(self.bytes.len())..];
        self.position = self.bytes.len();
        slice
    }
}

fn truncated() -> Error {
    Error::Protocol("truncated packet from the server".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_carries_a_little_endian_length_and_a_sequence_id() {
        let (bytes, next) = frame(b"select 1", 0);

        assert_eq!(&bytes[..3], &[8, 0, 0]);
        assert_eq!(bytes[3], 0);
        assert_eq!(&bytes[4..], b"select 1");
        assert_eq!(next, 1);
    }

    #[test]
    fn a_payload_longer_than_one_frame_is_split_and_terminated() {
        let payload = vec![0x41u8; MAX_PAYLOAD + 5];
        let (bytes, next) = frame(&payload, 3);

        // Two frames: a full one, then the remainder.
        assert_eq!(&bytes[..3], &[0xFF, 0xFF, 0xFF]);
        assert_eq!(bytes[3], 3);
        let second = &bytes[4 + MAX_PAYLOAD..];
        assert_eq!(&second[..3], &[5, 0, 0]);
        assert_eq!(second[3], 4);
        assert_eq!(next, 5);
    }

    #[test]
    fn a_length_encoded_integer_uses_each_of_its_four_widths() {
        // One byte, up to the first reserved marker.
        assert_eq!(Buffer::new().lenenc_int(250).clone_bytes(), vec![250]);
        // Two bytes behind 0xFC.
        assert_eq!(Buffer::new().lenenc_int(251).clone_bytes(), vec![0xFC, 251, 0]);
        assert_eq!(Buffer::new().lenenc_int(65_535).clone_bytes(), vec![0xFC, 0xFF, 0xFF]);
        // Three bytes behind 0xFD.
        assert_eq!(Buffer::new().lenenc_int(65_536).clone_bytes(), vec![0xFD, 0, 0, 1]);
        assert_eq!(
            Buffer::new().lenenc_int(16_777_215).clone_bytes(),
            vec![0xFD, 0xFF, 0xFF, 0xFF]
        );
        // Eight bytes behind 0xFE.
        assert_eq!(
            Buffer::new().lenenc_int(16_777_216).clone_bytes(),
            vec![0xFE, 0, 0, 0, 1, 0, 0, 0, 0]
        );
    }

    #[test]
    fn a_length_encoded_integer_survives_a_round_trip_at_every_width() {
        for value in [0u64, 1, 250, 251, 65_535, 65_536, 16_777_215, 16_777_216, u64::MAX] {
            let bytes = Buffer::new().lenenc_int(value).clone_bytes();
            let decoded = Reader::new(&bytes).lenenc_int().unwrap();
            assert_eq!(decoded, value, "for {value}");
        }
    }

    #[test]
    fn a_length_encoded_string_carries_its_own_length() {
        let bytes = Buffer::new().lenenc_bytes(b"ada").clone_bytes();
        assert_eq!(bytes, b"\x03ada");

        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.lenenc_bytes().unwrap(), b"ada");
    }

    #[test]
    fn parses_a_handshake_v10() {
        let mut payload = vec![10u8];
        payload.extend_from_slice(b"8.0.36\0");
        payload.extend_from_slice(&7u32.to_le_bytes());
        payload.extend_from_slice(b"12345678"); // scramble, part one
        payload.push(0); // filler
        payload.extend_from_slice(&((CLIENT_CAPABILITIES & 0xFFFF) as u16).to_le_bytes());
        payload.push(CHARSET_UTF8MB4);
        payload.extend_from_slice(&SERVER_STATUS_AUTOCOMMIT.to_le_bytes());
        payload.extend_from_slice(&((CLIENT_CAPABILITIES >> 16) as u16).to_le_bytes());
        payload.push(21); // total scramble length, including its NUL
        payload.extend_from_slice(&[0u8; 10]);
        payload.extend_from_slice(b"abcdefghijkl\0"); // scramble, part two
        payload.extend_from_slice(b"caching_sha2_password\0");

        let handshake = parse_handshake(&payload).unwrap();

        assert_eq!(handshake.server_version, "8.0.36");
        assert_eq!(handshake.connection_id, 7);
        assert_eq!(handshake.scramble, b"12345678abcdefghijkl");
        assert_eq!(handshake.auth_plugin, "caching_sha2_password");
        assert!(handshake.capabilities & CLIENT_PLUGIN_AUTH != 0);
    }

    #[test]
    fn refuses_a_handshake_from_a_server_too_old_to_talk_to() {
        let error = parse_handshake(&[9, 0]).unwrap_err().to_string();
        assert!(error.contains("version 10"), "{error}");
    }

    #[test]
    fn parses_an_ok_packet_including_the_generated_key() {
        // 0x00, affected=1, last insert id=42, status, warnings.
        let payload = [0x00, 0x01, 0x2A, 0x02, 0x00, 0x00, 0x00];

        let ok = parse_ok(&payload).unwrap();
        assert_eq!(ok.affected_rows, 1);
        assert_eq!(ok.last_insert_id, 42);
        assert_eq!(ok.status, SERVER_STATUS_AUTOCOMMIT);
    }

    #[test]
    fn an_error_packet_becomes_an_error_that_names_the_sql_state() {
        let mut payload = vec![0xFF];
        payload.extend_from_slice(&1146u16.to_le_bytes());
        payload.push(b'#');
        payload.extend_from_slice(b"42S02");
        payload.extend_from_slice(b"Table 'blog.nope' doesn't exist");

        let error = parse_err(&payload).unwrap();
        assert_eq!(error.code, 1146);
        assert_eq!(error.sql_state, "42S02");

        let rendered = error.into_error(Some("select * from nope")).to_string();
        assert!(rendered.contains("1146"), "{rendered}");
        assert!(rendered.contains("42S02"), "{rendered}");
        assert!(rendered.contains("SQL: select * from nope"), "{rendered}");
    }

    #[test]
    fn an_error_without_a_sql_state_still_reports_its_code() {
        let mut payload = vec![0xFF];
        payload.extend_from_slice(&1045u16.to_le_bytes());
        payload.extend_from_slice(b"Access denied");

        let error = parse_err(&payload).unwrap();
        assert_eq!(error.code, 1045);
        assert!(error.sql_state.is_empty());
        assert!(error.into_error(None).to_string().contains("1045"));
    }

    #[test]
    fn an_eof_packet_is_told_from_a_row_by_its_length() {
        let eof = [0xFE, 0x00, 0x00, 0x02, 0x00];
        assert!(is_eof(&eof));
        assert_eq!(parse_eof(&eof).unwrap().status, SERVER_STATUS_AUTOCOMMIT);

        // The same first byte in a longer packet is a length marker, not an EOF.
        let row = [0xFE, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        assert!(!is_eof(&row));
    }

    #[test]
    fn parses_a_column_definition() {
        let mut payload = Buffer::new();
        payload.lenenc_bytes(b"def");
        payload.lenenc_bytes(b"blog");
        payload.lenenc_bytes(b"users");
        payload.lenenc_bytes(b"users");
        payload.lenenc_bytes(b"total");
        payload.lenenc_bytes(b"id");
        payload.lenenc_int(0x0C);
        payload.u16(CHARSET_BINARY);
        payload.u32(20);
        payload.u8(types::LONGLONG);
        payload.u16(UNSIGNED_FLAG | NOT_NULL_FLAG);
        payload.u8(0);
        payload.u16(0);

        let column = parse_column(&payload.clone_bytes()).unwrap();

        // The alias, not the underlying column, because that is what a caller
        // asks for by name.
        assert_eq!(column.name, "total");
        assert_eq!(column.original_name, "id");
        assert_eq!(column.table, "users");
        assert_eq!(column.column_type, types::LONGLONG);
        assert!(column.is_unsigned());
        assert!(column.is_binary());
    }

    #[test]
    fn parses_a_prepare_response() {
        let mut payload = vec![0x00];
        payload.extend_from_slice(&9u32.to_le_bytes());
        payload.extend_from_slice(&3u16.to_le_bytes());
        payload.extend_from_slice(&2u16.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&0u16.to_le_bytes());

        let prepared = parse_prepare_ok(&payload).unwrap();
        assert_eq!(prepared.statement_id, 9);
        assert_eq!(prepared.columns, 3);
        assert_eq!(prepared.params, 2);
    }

    #[test]
    fn a_text_row_distinguishes_null_from_the_empty_string() {
        let payload = [0x03, b'a', b'd', b'a', 0xFB, 0x00];

        let values = parse_text_row(&payload, 3).unwrap();
        assert_eq!(values[0].as_deref(), Some(&b"ada"[..]));
        assert_eq!(values[1], None);
        assert_eq!(values[2].as_deref(), Some(&b""[..]));
    }

    #[test]
    fn a_handshake_response_names_the_plugin_and_the_database() {
        let bytes = Buffer::new()
            .handshake_response(
                CLIENT_CAPABILITIES | CLIENT_CONNECT_WITH_DB,
                "ada",
                &[0xAA; 20],
                Some("blog"),
                "mysql_native_password",
                &[("program_name", "rustlavel")],
            )
            .clone_bytes();

        let capabilities = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        assert!(capabilities & CLIENT_CONNECT_WITH_DB != 0);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), MAX_PACKET_SIZE);
        assert_eq!(bytes[8], CHARSET_UTF8MB4);
        assert_eq!(&bytes[9..32], &[0u8; 23]);
        assert_eq!(&bytes[32..36], b"ada\0");
        // Length-encoded auth response: 20 bytes of scramble.
        assert_eq!(bytes[36], 20);
        assert!(bytes.windows(5).any(|w| w == b"blog\0"));
        assert!(bytes.windows(22).any(|w| w == b"mysql_native_password\0"));
        assert!(bytes.windows(12).any(|w| w == b"program_name"));
    }

    #[test]
    fn a_nul_in_a_username_cannot_truncate_the_field() {
        let bytes = Buffer::new().cstr("ada\0admin").clone_bytes();
        assert_eq!(bytes, b"adaadmin\0");
    }

    #[test]
    fn execute_marks_null_parameters_in_a_bitmap_and_sends_no_value_for_them() {
        let bytes = Buffer::new()
            .com_stmt_execute(7, &[Value::Int(1), Value::Null, Value::Int(2)])
            .clone_bytes();

        assert_eq!(bytes[0], COM_STMT_EXECUTE);
        assert_eq!(u32::from_le_bytes(bytes[1..5].try_into().unwrap()), 7);
        assert_eq!(bytes[5], CURSOR_TYPE_NO_CURSOR);
        assert_eq!(u32::from_le_bytes(bytes[6..10].try_into().unwrap()), 1);

        // Three parameters fit in one bitmap byte; only the second is NULL.
        assert_eq!(bytes[10], 0b0000_0010);
        assert_eq!(bytes[11], 1, "new parameters are bound");

        // Three type pairs, then two 8-byte values — the NULL contributes none.
        assert_eq!(&bytes[12..18], &[types::LONGLONG, 0, types::NULL, 0, types::LONGLONG, 0]);
        assert_eq!(bytes.len(), 18 + 16);
    }

    #[test]
    fn execute_without_parameters_stops_before_the_bitmap() {
        let bytes = Buffer::new().com_stmt_execute(7, &[]).clone_bytes();
        assert_eq!(bytes.len(), 10);
    }

    #[test]
    fn builds_the_small_commands() {
        assert_eq!(Buffer::new().com_query("select 1").clone_bytes(), b"\x03select 1");
        assert_eq!(Buffer::new().com_ping().clone_bytes(), vec![COM_PING]);
        assert_eq!(Buffer::new().com_quit().clone_bytes(), vec![COM_QUIT]);
        assert_eq!(
            Buffer::new().com_stmt_prepare("select ?").clone_bytes(),
            b"\x16select ?"
        );
        assert_eq!(
            Buffer::new().com_stmt_close(5).clone_bytes(),
            vec![COM_STMT_CLOSE, 5, 0, 0, 0]
        );
    }

    #[test]
    fn a_truncated_packet_is_a_protocol_error_rather_than_a_panic() {
        assert!(parse_column(&[0x03, b'd']).is_err());
        assert!(parse_handshake(&[10]).is_err());
        assert!(Packet::parse(&[]).is_err());
        assert!(Reader::new(&[0xFC, 1]).lenenc_int().is_err());
    }

    #[test]
    fn classifies_the_packets_a_command_can_be_answered_with() {
        assert!(matches!(Packet::parse(&[0x00, 0, 0, 2, 0, 0, 0]).unwrap(), Packet::Ok(_)));
        assert!(matches!(Packet::parse(&[0xFF, 0x15, 0x04]).unwrap(), Packet::Err(_)));
        assert!(matches!(Packet::parse(&[0xFE, 0, 0, 2, 0]).unwrap(), Packet::Eof(_)));
        assert!(matches!(Packet::parse(&[0x01, 3]).unwrap(), Packet::AuthMoreData(data) if data == [3]));

        let mut switch = vec![0xFE];
        switch.extend_from_slice(b"mysql_native_password\0");
        switch.extend_from_slice(b"0123456789abcdefghij\0");
        match Packet::parse(&switch).unwrap() {
            Packet::AuthSwitch { plugin, data } => {
                assert_eq!(plugin, "mysql_native_password");
                assert_eq!(data, b"0123456789abcdefghij");
            }
            other => panic!("expected an auth switch, got {other:?}"),
        }
    }

    #[test]
    fn the_negotiated_capabilities_leave_out_the_dangerous_ones() {
        // Multi-statement would let one `COM_QUERY` carry two statements, and
        // LOAD DATA LOCAL would let the server read the client's disk.
        assert_eq!(CLIENT_CAPABILITIES & CLIENT_MULTI_STATEMENTS, 0);
        assert_eq!(CLIENT_CAPABILITIES & CLIENT_LOCAL_FILES, 0);
        // The ones the driver relies on are present.
        assert_ne!(CLIENT_CAPABILITIES & CLIENT_PROTOCOL_41, 0);
        assert_ne!(CLIENT_CAPABILITIES & CLIENT_PLUGIN_AUTH, 0);
        assert_ne!(CLIENT_CAPABILITIES & CLIENT_TRANSACTIONS, 0);
        // EOF packets are kept, so the result-set reader has one shape.
        assert_eq!(CLIENT_CAPABILITIES & CLIENT_DEPRECATE_EOF, 0);
        assert_eq!(CLIENT_CAPABILITIES & CLIENT_SSL, 0);
    }

    impl Buffer {
        fn clone_bytes(&self) -> Vec<u8> {
            self.bytes.clone()
        }
    }
}
