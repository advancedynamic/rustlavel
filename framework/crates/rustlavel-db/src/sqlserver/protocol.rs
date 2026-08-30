//! TDS — the Tabular Data Stream protocol SQL Server speaks.
//!
//! Everything travels in packets: an eight-byte header and a payload, with a
//! *message* being the run of packets ending in one whose status carries the
//! end-of-message bit. Requests are built as one payload and split by
//! [`split_message`]; responses are reassembled and then read as a stream of
//! tokens by [`TokenStream`].
//!
//! Unlike PostgreSQL, TDS is little-endian everywhere *except* the packet
//! header, whose length and SPID are network order. That single inconsistency
//! is the source of most first-attempt bugs, so the header has its own type
//! rather than being read inline.

use super::types::{self, Column};
use crate::value::Value;
use rustlavel_core::{Error, Result};
use std::sync::Arc;

/// The fixed size of a packet header.
pub const HEADER_LEN: usize = 8;

/// The packet size assumed before the server says otherwise.
///
/// The server may raise or lower it with an ENVCHANGE during login; until then
/// 4096 is the value MS-TDS specifies every implementation must accept.
pub const DEFAULT_PACKET_SIZE: usize = 4096;

/// TDS 7.4, which is what SQL Server 2012 and later speak.
pub const TDS_VERSION_7_4: u32 = 0x7400_0004;

/// Packet types, from the `Type` byte of the header.
pub mod packet {
    pub const SQL_BATCH: u8 = 0x01;
    pub const RPC: u8 = 0x03;
    pub const TABULAR_RESULT: u8 = 0x04;
    pub const ATTENTION: u8 = 0x06;
    pub const LOGIN7: u8 = 0x10;
    pub const SSPI: u8 = 0x11;
    pub const PRE_LOGIN: u8 = 0x12;
}

/// Status bits from the `Status` byte of the header.
pub mod status {
    pub const NORMAL: u8 = 0x00;
    pub const END_OF_MESSAGE: u8 = 0x01;
    pub const IGNORE: u8 = 0x02;
    pub const RESET_CONNECTION: u8 = 0x08;
}

/// The eight bytes in front of every packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub kind: u8,
    pub status: u8,
    /// Total packet length, header included.
    pub length: u16,
    pub spid: u16,
    /// Increments per packet within a message, wrapping at 255.
    pub id: u8,
    pub window: u8,
}

impl PacketHeader {
    pub fn parse(bytes: &[u8]) -> Result<PacketHeader> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::Protocol("truncated packet header from the server".into()));
        }
        Ok(PacketHeader {
            kind: bytes[0],
            status: bytes[1],
            // Length and SPID are the only big-endian fields in all of TDS.
            length: u16::from_be_bytes([bytes[2], bytes[3]]),
            spid: u16::from_be_bytes([bytes[4], bytes[5]]),
            id: bytes[6],
            window: bytes[7],
        })
    }

    pub fn write_into(&self, out: &mut Vec<u8>) {
        out.push(self.kind);
        out.push(self.status);
        out.extend_from_slice(&self.length.to_be_bytes());
        out.extend_from_slice(&self.spid.to_be_bytes());
        out.push(self.id);
        out.push(self.window);
    }

    pub fn is_end_of_message(&self) -> bool {
        self.status & status::END_OF_MESSAGE != 0
    }
}

/// Frame a payload as one or more packets of at most `packet_size` bytes.
///
/// Only the final packet carries the end-of-message bit, which is how the peer
/// knows a login or a statement longer than one packet is complete. The packet
/// id restarts at one for every message, exactly as MS-TDS requires.
///
/// Each packet comes back as its own buffer rather than one concatenated block,
/// and that is not a stylistic choice. **Over an encrypted connection SQL Server
/// expects one TDS packet per TLS record**: give it a record holding two
/// packets and it drops the connection without a word — a live server confirmed
/// it, at exactly the payload size where a second packet appears, and only when
/// encryption was on. Writing each packet separately puts each in its own
/// record, which is what every working TDS client does.
pub fn split_message(kind: u8, payload: &[u8], packet_size: usize) -> Vec<Vec<u8>> {
    let capacity = packet_size.max(HEADER_LEN + 1) - HEADER_LEN;
    let mut packets = Vec::with_capacity(payload.len() / capacity + 1);
    let mut id: u8 = 1;

    // An empty payload is still a message, so the loop runs at least once.
    let mut offset = 0;
    loop {
        let end = (offset + capacity).min(payload.len());
        let chunk = &payload[offset..end];
        let last = end == payload.len();

        let mut packet = Vec::with_capacity(HEADER_LEN + chunk.len());
        PacketHeader {
            kind,
            status: if last { status::END_OF_MESSAGE } else { status::NORMAL },
            length: (HEADER_LEN + chunk.len()) as u16,
            spid: 0,
            id,
            window: 0,
        }
        .write_into(&mut packet);
        packet.extend_from_slice(chunk);
        packets.push(packet);

        if last {
            return packets;
        }
        offset = end;
        id = id.wrapping_add(1);
    }
}

// --- PRELOGIN ---

/// PRELOGIN option tokens.
pub mod prelogin_option {
    pub const VERSION: u8 = 0x00;
    pub const ENCRYPTION: u8 = 0x01;
    pub const INSTOPT: u8 = 0x02;
    pub const THREADID: u8 = 0x03;
    pub const MARS: u8 = 0x04;
    pub const TERMINATOR: u8 = 0xFF;
}

/// The values the ENCRYPTION option can carry, in both directions.
pub mod encryption {
    /// Available but off: only the login packet is encrypted.
    pub const OFF: u8 = 0x00;
    /// On for the whole session.
    pub const ON: u8 = 0x01;
    /// This side cannot do encryption at all.
    pub const NOT_SUPPORTED: u8 = 0x02;
    /// The server insists on it.
    pub const REQUIRED: u8 = 0x03;
}

/// Build a PRELOGIN payload asking for a particular encryption level.
///
/// Every option is a five-byte entry — token, offset, length — and the offsets
/// are counted from the start of this payload, which is why the option data
/// cannot be written until the whole option header is sized.
pub fn prelogin(encryption: u8) -> Vec<u8> {
    let options: [(u8, Vec<u8>); 5] = [
        // A version the server will accept; it does not gate anything.
        (prelogin_option::VERSION, vec![9, 0, 0, 0, 0, 0]),
        (prelogin_option::ENCRYPTION, vec![encryption]),
        // No named instance: an empty, null-terminated instance name.
        (prelogin_option::INSTOPT, vec![0]),
        (prelogin_option::THREADID, 0u32.to_le_bytes().to_vec()),
        // Multiple Active Result Sets off: one statement at a time per
        // connection is exactly what the pool already guarantees.
        (prelogin_option::MARS, vec![0]),
    ];

    let header_len = options.len() * 5 + 1;
    let mut head = Vec::with_capacity(header_len);
    let mut data = Vec::new();

    for (token, value) in &options {
        head.push(*token);
        head.extend_from_slice(&((header_len + data.len()) as u16).to_be_bytes());
        head.extend_from_slice(&(value.len() as u16).to_be_bytes());
        data.extend_from_slice(value);
    }
    head.push(prelogin_option::TERMINATOR);

    head.extend_from_slice(&data);
    head
}

/// What the server answered in its PRELOGIN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreloginResponse {
    pub encryption: u8,
}

pub fn parse_prelogin(payload: &[u8]) -> Result<PreloginResponse> {
    let mut encryption = encryption::NOT_SUPPORTED;
    let mut at = 0;

    while at < payload.len() {
        let token = payload[at];
        if token == prelogin_option::TERMINATOR {
            break;
        }
        if at + 5 > payload.len() {
            return Err(Error::Protocol("truncated PRELOGIN option header".into()));
        }
        let offset = u16::from_be_bytes([payload[at + 1], payload[at + 2]]) as usize;
        let length = u16::from_be_bytes([payload[at + 3], payload[at + 4]]) as usize;
        if offset + length > payload.len() {
            return Err(Error::Protocol("PRELOGIN option points past the packet".into()));
        }
        if token == prelogin_option::ENCRYPTION && length >= 1 {
            encryption = payload[offset];
        }
        at += 5;
    }

    Ok(PreloginResponse { encryption })
}

// --- LOGIN7 ---

/// Everything LOGIN7 carries that is not a constant.
#[derive(Debug, Clone)]
pub struct Login7<'a> {
    pub hostname: &'a str,
    pub username: &'a str,
    /// Already obfuscated by [`super::auth::obfuscate_password`].
    pub password: &'a [u8],
    pub application: &'a str,
    pub server: &'a str,
    pub library: &'a str,
    pub language: &'a str,
    pub database: &'a str,
    pub packet_size: usize,
}

/// The size of the fixed part of LOGIN7 in TDS 7.4, which is also where the
/// variable-length data starts.
const LOGIN7_FIXED_LEN: usize = 94;

/// fUseDB + fDatabase + fSetLang: the server reports a database or language
/// change, and refuses the connection outright if the requested database
/// cannot be opened — a silent fallback to `master` is far worse than an error.
const OPTION_FLAGS_1: u8 = 0xE0;

/// fODBC, which asks the server for ANSI defaults (quoted identifiers, ANSI
/// nulls, ANSI warnings) rather than the legacy ones.
const OPTION_FLAGS_2: u8 = 0x02;

pub fn login7(login: &Login7<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);

    // Reserved for the total length, patched once everything is written.
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&TDS_VERSION_7_4.to_le_bytes());
    out.extend_from_slice(&(login.packet_size as u32).to_le_bytes());
    out.extend_from_slice(&0x0100_0000u32.to_le_bytes()); // client program version
    out.extend_from_slice(&std::process::id().to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // connection id
    out.push(OPTION_FLAGS_1);
    out.push(OPTION_FLAGS_2);
    out.push(0); // type flags: a plain SQL client, read-write
    out.push(0); // option flags 3: no feature extension
    out.extend_from_slice(&0i32.to_le_bytes()); // client time zone
    out.extend_from_slice(&0u32.to_le_bytes()); // client LCID: server default

    // Strings live after the fixed part; each is referenced by an offset from
    // the start of the payload and a length counted in *characters*, not bytes.
    let mut data: Vec<u8> = Vec::new();
    let place = |data: &mut Vec<u8>, text: &str| -> [u8; 4] {
        let offset = (LOGIN7_FIXED_LEN + data.len()) as u16;
        let mut characters = 0u16;
        for unit in text.encode_utf16() {
            data.extend_from_slice(&unit.to_le_bytes());
            characters += 1;
        }
        let mut entry = [0u8; 4];
        entry[..2].copy_from_slice(&offset.to_le_bytes());
        entry[2..].copy_from_slice(&characters.to_le_bytes());
        entry
    };

    let hostname = place(&mut data, login.hostname);
    let username = place(&mut data, login.username);

    // The password is placed by hand: it is already bytes, and its length is
    // still counted in UTF-16 characters, so it is half the byte count.
    let password_offset = (LOGIN7_FIXED_LEN + data.len()) as u16;
    data.extend_from_slice(login.password);
    let password_characters = (login.password.len() / 2) as u16;

    let application = place(&mut data, login.application);
    let server = place(&mut data, login.server);
    let library = place(&mut data, login.library);
    let language = place(&mut data, login.language);
    let database = place(&mut data, login.database);
    let tail = (LOGIN7_FIXED_LEN + data.len()) as u16;

    out.extend_from_slice(&hostname);
    out.extend_from_slice(&username);
    out.extend_from_slice(&password_offset.to_le_bytes());
    out.extend_from_slice(&password_characters.to_le_bytes());
    out.extend_from_slice(&application);
    out.extend_from_slice(&server);
    out.extend_from_slice(&[0u8; 4]); // no extension block
    out.extend_from_slice(&library);
    out.extend_from_slice(&language);
    out.extend_from_slice(&database);
    out.extend_from_slice(&[0u8; 6]); // client MAC address, which nothing reads
    out.extend_from_slice(&tail.to_le_bytes()); // ibSSPI
    out.extend_from_slice(&0u16.to_le_bytes()); // cbSSPI: SQL authentication only
    out.extend_from_slice(&tail.to_le_bytes()); // ibAtchDBFile
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&tail.to_le_bytes()); // ibChangePassword
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // cbSSPILong

    debug_assert_eq!(out.len(), LOGIN7_FIXED_LEN);
    out.extend_from_slice(&data);

    let length = out.len() as u32;
    out[..4].copy_from_slice(&length.to_le_bytes());
    out
}

// --- Requests ---

/// The ALL_HEADERS block every batch and RPC carries in TDS 7.2 and later.
///
/// It exists to name the transaction the request belongs to; getting it wrong
/// makes the server reject the request rather than run it outside the
/// transaction, which is the safer of the two failure modes.
pub fn all_headers(transaction: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(22);
    out.extend_from_slice(&22u32.to_le_bytes()); // total length, itself included
    out.extend_from_slice(&18u32.to_le_bytes()); // this header's length
    out.extend_from_slice(&2u16.to_le_bytes()); // transaction descriptor header
    out.extend_from_slice(&transaction.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // outstanding request count
    out
}

/// A statement with no parameters, sent as text.
pub fn sql_batch(sql: &str, transaction: u64) -> Vec<u8> {
    let mut out = all_headers(transaction);
    for unit in sql.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// The well-known procedure id of `sp_executesql`.
///
/// Sending the id rather than the name saves the server a name lookup, and is
/// what every production TDS client does.
pub const SP_EXECUTESQL: u16 = 10;

/// One parameter of an RPC call: a name and its already-encoded type and value.
#[derive(Debug, Clone)]
pub struct RpcParameter {
    pub name: String,
    /// TYPE_INFO followed by TYPE_VARBYTE, as [`types::encode`] produces.
    pub bytes: Vec<u8>,
}

pub fn rpc(proc_id: u16, parameters: &[RpcParameter], transaction: u64) -> Vec<u8> {
    let mut out = all_headers(transaction);
    // 0xFFFF says "a procedure id follows" rather than a name.
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&proc_id.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // option flags

    for parameter in parameters {
        let name: Vec<u16> = parameter.name.encode_utf16().collect();
        out.push(name.len() as u8);
        for unit in &name {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.push(0); // status flags: an input parameter
        out.extend_from_slice(&parameter.bytes);
    }

    out
}

/// Build the `sp_executesql` call for a parameterised statement.
///
/// The statement text and the parameter declarations are themselves
/// parameters, so nothing a caller binds is ever concatenated into SQL. This is
/// the whole reason the driver takes the RPC route instead of the far simpler
/// batch one.
pub fn execute_sql(sql: &str, params: &[Value], transaction: u64) -> Vec<u8> {
    let declaration = types::declare(params);

    let mut parameters = Vec::with_capacity(params.len() + 2);
    parameters.push(RpcParameter {
        name: String::new(),
        bytes: types::encode(&Value::Text(sql.to_string())),
    });
    parameters.push(RpcParameter {
        name: String::new(),
        bytes: types::encode(&Value::Text(declaration)),
    });
    for (index, value) in params.iter().enumerate() {
        parameters.push(RpcParameter {
            name: format!("@P{}", index + 1),
            bytes: types::encode(value),
        });
    }

    rpc(SP_EXECUTESQL, &parameters, transaction)
}

// --- The token stream ---

/// Token identifiers from the response stream.
pub mod token {
    pub const RETURN_STATUS: u8 = 0x79;
    pub const COLMETADATA: u8 = 0x81;
    pub const ALTMETADATA: u8 = 0x88;
    pub const TABNAME: u8 = 0xA4;
    pub const COLINFO: u8 = 0xA5;
    pub const ORDER: u8 = 0xA9;
    pub const ERROR: u8 = 0xAA;
    pub const INFO: u8 = 0xAB;
    pub const RETURN_VALUE: u8 = 0xAC;
    pub const LOGINACK: u8 = 0xAD;
    pub const FEATUREEXTACK: u8 = 0xAE;
    pub const ROW: u8 = 0xD1;
    pub const NBCROW: u8 = 0xD2;
    pub const ENVCHANGE: u8 = 0xE3;
    pub const SSPI: u8 = 0xED;
    pub const DONE: u8 = 0xFD;
    pub const DONEPROC: u8 = 0xFE;
    pub const DONEINPROC: u8 = 0xFF;
}

/// An error or an informational message from the server.
///
/// The two share a wire format; only the token byte and the severity separate
/// a failure from a `print` statement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerError {
    pub number: i32,
    pub state: u8,
    /// SQL Server calls this the class; `raiserror` calls it the severity.
    /// Anything above 10 is a real error.
    pub severity: u8,
    pub message: String,
    pub server: String,
    pub procedure: String,
    pub line: u32,
}

impl ServerError {
    pub fn into_error(self, sql: Option<&str>) -> Error {
        let mut text = format!(
            "SQL Server error {} (severity {}, state {}): {}",
            self.number, self.severity, self.state, self.message
        );
        if !self.procedure.is_empty() {
            text.push_str(&format!(" — in {}, line {}", self.procedure, self.line));
        }
        // Pointing at the offending statement is the difference between a
        // usable error and a puzzle.
        if let Some(sql) = sql {
            text.push_str(&format!("\n  SQL: {sql}"));
        }
        Error::msg(text)
    }
}

/// Which of the three DONE tokens arrived.
///
/// They are the same shape but mean different things: DONEINPROC ends one
/// statement inside a procedure, DONEPROC ends the procedure, DONE ends a
/// batch. Only the first carries a row count for a statement run through
/// `sp_executesql`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneKind {
    Batch,
    Procedure,
    InProcedure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Done {
    pub kind: DoneKind,
    pub status: u16,
    pub current_command: u16,
    pub rows: u64,
}

impl Done {
    /// DONE_COUNT: whether `rows` means anything at all.
    pub fn has_count(&self) -> bool {
        self.status & 0x0010 != 0
    }

    /// DONE_ERROR: the statement failed. An ERROR token said why.
    pub fn has_error(&self) -> bool {
        self.status & 0x0002 != 0
    }

    /// DONE_MORE: another result set follows in this same message.
    pub fn has_more(&self) -> bool {
        self.status & 0x0001 != 0
    }
}

/// The LOGINACK that says the credentials were accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginAck {
    pub interface: u8,
    pub tds_version: u32,
    pub program: String,
    pub version: (u8, u8, u16),
}

/// A change of session state the server announces rather than being asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvChange {
    Database(String),
    PacketSize(usize),
    /// The descriptor every later request must quote in its ALL_HEADERS.
    BeginTransaction(u64),
    CommitTransaction,
    RollbackTransaction,
    /// A change the driver does not act on, named by its type byte.
    Other(u8),
}

#[derive(Debug, Clone)]
pub enum Token {
    /// A new result set is starting; the columns describe every row after it.
    ColumnMetadata(Arc<Vec<Column>>),
    Row(Vec<Value>),
    Done(Done),
    Error(ServerError),
    Info(ServerError),
    LoginAck(LoginAck),
    EnvChange(EnvChange),
    ReturnStatus(i32),
    /// A token that was parsed far enough to skip it safely.
    Ignored(u8),
}

/// Reads a reassembled response message one token at a time.
///
/// The stream is stateful because rows carry no types of their own: they are
/// decoded against the COLMETADATA that preceded them.
pub struct TokenStream<'a> {
    reader: Reader<'a>,
    columns: Arc<Vec<Column>>,
}

impl<'a> TokenStream<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        TokenStream { reader: Reader::new(bytes), columns: Arc::new(Vec::new()) }
    }

    /// The columns of the result set currently being read.
    pub fn columns(&self) -> &Arc<Vec<Column>> {
        &self.columns
    }

    /// The next token, or `None` at the end of the message.
    ///
    /// Not an `Iterator`: a token can fail to parse, and a stream that hides
    /// that behind `None` would silently truncate a result set.
    pub fn next_token(&mut self) -> Result<Option<Token>> {
        if self.reader.is_empty() {
            return Ok(None);
        }

        let tag = self.reader.u8()?;
        let parsed = match tag {
            token::COLMETADATA => {
                self.columns = Arc::new(types::parse_column_metadata(&mut self.reader)?);
                Token::ColumnMetadata(Arc::clone(&self.columns))
            }
            token::ROW => Token::Row(types::read_row(&mut self.reader, &self.columns)?),
            token::NBCROW => Token::Row(types::read_nbc_row(&mut self.reader, &self.columns)?),
            token::DONE => Token::Done(parse_done(DoneKind::Batch, &mut self.reader)?),
            token::DONEPROC => Token::Done(parse_done(DoneKind::Procedure, &mut self.reader)?),
            token::DONEINPROC => Token::Done(parse_done(DoneKind::InProcedure, &mut self.reader)?),
            token::ERROR => Token::Error(parse_server_error(&mut self.reader)?),
            token::INFO => Token::Info(parse_server_error(&mut self.reader)?),
            token::LOGINACK => Token::LoginAck(parse_login_ack(&mut self.reader)?),
            token::ENVCHANGE => Token::EnvChange(parse_env_change(&mut self.reader)?),
            token::RETURN_STATUS => Token::ReturnStatus(self.reader.i32()?),
            token::RETURN_VALUE => {
                skip_return_value(&mut self.reader)?;
                Token::Ignored(tag)
            }
            token::FEATUREEXTACK => {
                skip_feature_ext_ack(&mut self.reader)?;
                Token::Ignored(tag)
            }
            // Length-prefixed tokens the driver has no use for.
            token::ORDER | token::TABNAME | token::COLINFO | token::SSPI | token::ALTMETADATA => {
                let length = self.reader.u16()? as usize;
                self.reader.skip(length)?;
                Token::Ignored(tag)
            }
            other => {
                // Guessing at an unknown token's length would silently
                // desynchronise the whole stream; saying so is safer.
                return Err(Error::Protocol(format!(
                    "unknown TDS token 0x{other:02X} in the response stream"
                )));
            }
        };

        Ok(Some(parsed))
    }
}

fn parse_done(kind: DoneKind, reader: &mut Reader<'_>) -> Result<Done> {
    Ok(Done {
        kind,
        status: reader.u16()?,
        current_command: reader.u16()?,
        rows: reader.u64()?,
    })
}

fn parse_server_error(reader: &mut Reader<'_>) -> Result<ServerError> {
    // The length covers everything after itself; the fields are read rather
    // than sliced, so a short token becomes a protocol error either way.
    let _length = reader.u16()?;
    Ok(ServerError {
        number: reader.i32()?,
        state: reader.u8()?,
        severity: reader.u8()?,
        message: reader.us_varchar()?,
        server: reader.b_varchar()?,
        procedure: reader.b_varchar()?,
        line: reader.u32()?,
    })
}

fn parse_login_ack(reader: &mut Reader<'_>) -> Result<LoginAck> {
    let _length = reader.u16()?;
    let interface = reader.u8()?;
    let tds_version = reader.u32()?;
    let program = reader.b_varchar()?;
    let major = reader.u8()?;
    let minor = reader.u8()?;
    let build_high = reader.u8()?;
    let build_low = reader.u8()?;

    Ok(LoginAck {
        interface,
        tds_version,
        program,
        version: (major, minor, u16::from_be_bytes([build_high, build_low])),
    })
}

fn parse_env_change(reader: &mut Reader<'_>) -> Result<EnvChange> {
    let length = reader.u16()? as usize;
    let body = reader.take(length)?;
    let mut inner = Reader::new(body);

    Ok(match inner.u8()? {
        1 => EnvChange::Database(inner.b_varchar()?),
        // The negotiated packet size arrives as a decimal string, not a number.
        4 => EnvChange::PacketSize(
            inner.b_varchar()?.parse().unwrap_or(DEFAULT_PACKET_SIZE),
        ),
        8 => {
            let descriptor = inner.b_varbyte()?;
            let mut bytes = [0u8; 8];
            let taken = descriptor.len().min(8);
            bytes[..taken].copy_from_slice(&descriptor[..taken]);
            EnvChange::BeginTransaction(u64::from_le_bytes(bytes))
        }
        9 => EnvChange::CommitTransaction,
        10 => EnvChange::RollbackTransaction,
        other => EnvChange::Other(other),
    })
}

/// Consume a RETURNVALUE token. `sp_executesql` is called without output
/// parameters, so its value is read only to keep the stream aligned.
fn skip_return_value(reader: &mut Reader<'_>) -> Result<()> {
    reader.u16()?; // parameter ordinal
    reader.b_varchar()?; // parameter name
    reader.u8()?; // status
    reader.u32()?; // user type
    reader.u16()?; // flags
    let type_info = types::parse_type_info(reader)?;
    types::read_value(reader, &type_info)?;
    Ok(())
}

/// Consume a FEATUREEXTACK token, which is a list terminated by 0xFF.
fn skip_feature_ext_ack(reader: &mut Reader<'_>) -> Result<()> {
    loop {
        if reader.u8()? == 0xFF {
            return Ok(());
        }
        let length = reader.u32()? as usize;
        reader.skip(length)?;
    }
}

/// A cursor over a reassembled message.
///
/// Every multi-byte field in TDS below the packet header is little-endian, so
/// unlike the PostgreSQL reader this one never sees a big-endian integer.
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

    pub fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(count).ok_or_else(too_short)?;
        if end > self.bytes.len() {
            return Err(too_short());
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

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    /// A string with a one-byte character count.
    pub fn b_varchar(&mut self) -> Result<String> {
        let characters = self.u8()? as usize;
        self.ucs2(characters)
    }

    /// A string with a two-byte character count.
    pub fn us_varchar(&mut self) -> Result<String> {
        let characters = self.u16()? as usize;
        self.ucs2(characters)
    }

    /// A byte string with a one-byte length.
    pub fn b_varbyte(&mut self) -> Result<&'a [u8]> {
        let length = self.u8()? as usize;
        self.take(length)
    }

    fn ucs2(&mut self, characters: usize) -> Result<String> {
        let bytes = self.take(characters * 2)?;
        Ok(decode_ucs2(bytes))
    }
}

/// Decode UTF-16LE, which TDS calls UCS-2 and uses for every string it sends.
pub fn decode_ucs2(bytes: &[u8]) -> String {
    // `as_chunks` rather than `chunks_exact(2)`: the pair arrives as a fixed
    // `[u8; 2]`, so `from_le_bytes` needs no bounds check and no copy. A
    // trailing odd byte is dropped either way, which is right — half a UTF-16
    // code unit is not a character.
    let (pairs, _odd_trailing_byte) = bytes.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().copied().map(u16::from_le_bytes).collect();
    String::from_utf16_lossy(&units)
}

fn too_short() -> Error {
    Error::Protocol("truncated message from the server".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packet_header_survives_a_round_trip() {
        let header = PacketHeader {
            kind: packet::SQL_BATCH,
            status: status::END_OF_MESSAGE,
            length: 4096,
            spid: 53,
            id: 7,
            window: 0,
        };

        let mut bytes = Vec::new();
        header.write_into(&mut bytes);

        assert_eq!(bytes.len(), HEADER_LEN);
        // Length and SPID are big-endian even though the rest of TDS is not.
        assert_eq!(&bytes[2..4], &4096u16.to_be_bytes());
        assert_eq!(PacketHeader::parse(&bytes).unwrap(), header);
    }

    #[test]
    fn a_short_header_is_a_protocol_error() {
        assert!(PacketHeader::parse(&[0x04, 0x01, 0x00]).is_err());
    }

    #[test]
    fn a_payload_that_fits_becomes_one_packet_marked_final() {
        let packets = split_message(packet::SQL_BATCH, b"hello", DEFAULT_PACKET_SIZE);
        assert_eq!(packets.len(), 1);

        let header = PacketHeader::parse(&packets[0]).unwrap();
        assert_eq!(header.kind, packet::SQL_BATCH);
        assert_eq!(header.length as usize, packets[0].len());
        assert_eq!(header.id, 1);
        assert!(header.is_end_of_message());
        assert_eq!(&packets[0][HEADER_LEN..], b"hello");
    }

    #[test]
    fn a_payload_larger_than_the_packet_size_is_split_and_only_the_last_ends_it() {
        // Three packets: 8 bytes of header leaves 24 bytes of room in each.
        let payload: Vec<u8> = (0..60u8).collect();
        let packets = split_message(packet::SQL_BATCH, &payload, 32);

        assert_eq!(packets.len(), 3);

        let headers: Vec<PacketHeader> =
            packets.iter().map(|p| PacketHeader::parse(p).unwrap()).collect();
        assert_eq!(headers.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(!headers[0].is_end_of_message());
        assert!(!headers[1].is_end_of_message());
        assert!(headers[2].is_end_of_message());
        // None exceeds the negotiated size, header included.
        assert!(packets.iter().all(|p| p.len() <= 32));

        // Split and reassembled, the payload is unchanged.
        let rebuilt: Vec<u8> =
            packets.iter().flat_map(|p| p[HEADER_LEN..].iter().copied()).collect();
        assert_eq!(rebuilt, payload);
    }

    #[test]
    fn an_empty_payload_is_still_one_end_of_message_packet() {
        let packets = split_message(packet::PRE_LOGIN, &[], DEFAULT_PACKET_SIZE);

        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].len(), HEADER_LEN);
        assert!(PacketHeader::parse(&packets[0]).unwrap().is_end_of_message());
    }

    #[test]
    fn a_prelogin_requests_encryption_and_its_offsets_point_at_its_data() {
        let payload = prelogin(encryption::ON);

        // Five options of five bytes each, then the terminator.
        assert_eq!(payload[25], prelogin_option::TERMINATOR);
        assert_eq!(payload[5], prelogin_option::ENCRYPTION);

        let offset = u16::from_be_bytes([payload[6], payload[7]]) as usize;
        let length = u16::from_be_bytes([payload[8], payload[9]]) as usize;
        assert_eq!(length, 1);
        assert_eq!(payload[offset], encryption::ON);
    }

    #[test]
    fn reads_the_encryption_level_the_server_chose() {
        let mut answer = prelogin(encryption::REQUIRED);
        assert_eq!(
            parse_prelogin(&answer).unwrap(),
            PreloginResponse { encryption: encryption::REQUIRED }
        );

        // A response with no ENCRYPTION option at all means no encryption.
        answer.truncate(1);
        answer[0] = prelogin_option::TERMINATOR;
        assert_eq!(
            parse_prelogin(&answer).unwrap().encryption,
            encryption::NOT_SUPPORTED
        );
    }

    #[test]
    fn a_prelogin_option_pointing_past_the_packet_is_rejected() {
        let payload = vec![prelogin_option::ENCRYPTION, 0xFF, 0xFF, 0x00, 0x01, 0xFF];
        assert!(parse_prelogin(&payload).is_err());
    }

    #[test]
    fn login7_declares_its_own_length_and_counts_strings_in_characters() {
        let payload = login7(&Login7 {
            hostname: "laptop",
            username: "sa",
            password: &[0xB3, 0xA5, 0x83, 0xA5],
            application: "rustlavel",
            server: "db",
            library: "rustlavel-db",
            language: "",
            database: "blog",
            packet_size: DEFAULT_PACKET_SIZE,
        });

        assert_eq!(
            u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize,
            payload.len()
        );
        assert_eq!(u32::from_le_bytes(payload[4..8].try_into().unwrap()), TDS_VERSION_7_4);

        // The username entry: offset then a count of characters, not bytes.
        let username_offset = u16::from_le_bytes(payload[40..42].try_into().unwrap()) as usize;
        let username_length = u16::from_le_bytes(payload[42..44].try_into().unwrap()) as usize;
        assert_eq!(username_length, 2);
        assert_eq!(
            decode_ucs2(&payload[username_offset..username_offset + username_length * 2]),
            "sa"
        );

        // The password is two UTF-16 characters, so four bytes.
        let password_length = u16::from_le_bytes(payload[46..48].try_into().unwrap());
        assert_eq!(password_length, 2);
    }

    #[test]
    fn a_batch_carries_the_transaction_it_belongs_to() {
        let payload = sql_batch("select 1", 0xDEAD_BEEF);

        assert_eq!(u32::from_le_bytes(payload[..4].try_into().unwrap()), 22);
        assert_eq!(u64::from_le_bytes(payload[10..18].try_into().unwrap()), 0xDEAD_BEEF);
        assert_eq!(decode_ucs2(&payload[22..]), "select 1");
    }

    #[test]
    fn an_rpc_names_its_procedure_by_id() {
        let payload = rpc(SP_EXECUTESQL, &[], 0);

        assert_eq!(u16::from_le_bytes(payload[22..24].try_into().unwrap()), 0xFFFF);
        assert_eq!(u16::from_le_bytes(payload[24..26].try_into().unwrap()), SP_EXECUTESQL);
    }

    #[test]
    fn a_parameterised_call_sends_the_statement_as_data_not_as_sql() {
        let hostile = "'; drop table users; --";
        let payload = execute_sql("select @P1", &[Value::Text(hostile.into())], 0);

        // The statement text appears once, and the hostile value appears as a
        // separate UTF-16 parameter — never spliced into the statement.
        let statement: Vec<u8> = "select @P1".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let value: Vec<u8> = hostile.encode_utf16().flat_map(u16::to_le_bytes).collect();

        assert!(payload.windows(statement.len()).any(|w| w == statement));
        assert!(payload.windows(value.len()).any(|w| w == value));

        // The declaration names the parameter and its type, so the server binds
        // it rather than parsing it.
        let declaration: Vec<u8> =
            "@P1 nvarchar(max)".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(payload.windows(declaration.len()).any(|w| w == declaration));
    }

    #[test]
    fn an_error_token_names_its_number_and_severity() {
        let mut body = vec![token::ERROR];
        let mut fields = Vec::new();
        fields.extend_from_slice(&18456i32.to_le_bytes());
        fields.push(1); // state
        fields.push(14); // severity
        let message = "Login failed for user 'sa'.";
        fields.extend_from_slice(&(message.encode_utf16().count() as u16).to_le_bytes());
        fields.extend(message.encode_utf16().flat_map(u16::to_le_bytes));
        fields.push(2); // server name length
        fields.extend("db".encode_utf16().flat_map(u16::to_le_bytes));
        fields.push(0); // no procedure
        fields.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        body.extend_from_slice(&fields);

        let mut stream = TokenStream::new(&body);
        let error = match stream.next_token().unwrap().unwrap() {
            Token::Error(error) => error,
            other => panic!("expected an error token, got {other:?}"),
        };

        assert_eq!(error.number, 18456);
        assert_eq!(error.severity, 14);
        assert_eq!(error.message, message);
        assert_eq!(error.server, "db");

        let rendered = error.into_error(Some("select 1")).to_string();
        assert!(rendered.contains("18456"), "{rendered}");
        assert!(rendered.contains("severity 14"), "{rendered}");
        assert!(rendered.contains("SQL: select 1"), "{rendered}");
    }

    #[test]
    fn a_done_token_reports_the_rows_a_statement_touched() {
        let mut body = vec![token::DONEINPROC];
        body.extend_from_slice(&0x0011u16.to_le_bytes()); // DONE_MORE | DONE_COUNT
        body.extend_from_slice(&0xC1u16.to_le_bytes()); // current command
        body.extend_from_slice(&3u64.to_le_bytes());

        let mut stream = TokenStream::new(&body);
        let done = match stream.next_token().unwrap().unwrap() {
            Token::Done(done) => done,
            other => panic!("expected a done token, got {other:?}"),
        };

        assert_eq!(done.kind, DoneKind::InProcedure);
        assert_eq!(done.rows, 3);
        assert!(done.has_count());
        assert!(done.has_more());
        assert!(!done.has_error());
    }

    #[test]
    fn a_done_token_without_a_count_bit_reports_no_rows() {
        let mut body = vec![token::DONE];
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&99u64.to_le_bytes());

        let mut stream = TokenStream::new(&body);
        match stream.next_token().unwrap().unwrap() {
            // The count is still on the wire; `has_count` is what says to trust it.
            Token::Done(done) => assert!(!done.has_count()),
            other => panic!("expected a done token, got {other:?}"),
        }
    }

    #[test]
    fn an_env_change_announces_a_transaction_and_then_ends_it() {
        let mut body = vec![token::ENVCHANGE];
        let mut change = vec![8u8]; // begin transaction
        change.push(8); // descriptor length
        change.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        change.push(0); // no old value
        body.extend_from_slice(&(change.len() as u16).to_le_bytes());
        body.extend_from_slice(&change);

        body.push(token::ENVCHANGE);
        let ended = vec![9u8, 0, 0];
        body.extend_from_slice(&(ended.len() as u16).to_le_bytes());
        body.extend_from_slice(&ended);

        let mut stream = TokenStream::new(&body);
        match stream.next_token().unwrap().unwrap() {
            Token::EnvChange(EnvChange::BeginTransaction(descriptor)) => {
                assert_eq!(descriptor, 0x0102_0304_0506_0708)
            }
            other => panic!("expected a transaction to begin, got {other:?}"),
        }
        match stream.next_token().unwrap().unwrap() {
            Token::EnvChange(EnvChange::CommitTransaction) => {}
            other => panic!("expected a commit, got {other:?}"),
        }
    }

    #[test]
    fn the_negotiated_packet_size_arrives_as_a_decimal_string() {
        let mut body = vec![token::ENVCHANGE];
        let mut change = vec![4u8];
        change.push(4); // "8192" is four characters
        change.extend("8192".encode_utf16().flat_map(u16::to_le_bytes));
        change.push(0);
        body.extend_from_slice(&(change.len() as u16).to_le_bytes());
        body.extend_from_slice(&change);

        let mut stream = TokenStream::new(&body);
        match stream.next_token().unwrap().unwrap() {
            Token::EnvChange(EnvChange::PacketSize(size)) => assert_eq!(size, 8192),
            other => panic!("expected a packet size change, got {other:?}"),
        }
    }

    #[test]
    fn a_login_ack_reports_the_version_that_was_negotiated() {
        let mut fields = vec![1u8]; // interface
        fields.extend_from_slice(&TDS_VERSION_7_4.to_le_bytes());
        fields.push(4);
        fields.extend("mssq".encode_utf16().flat_map(u16::to_le_bytes));
        fields.extend_from_slice(&[16, 0, 0x0F, 0xA0]);

        let mut body = vec![token::LOGINACK];
        body.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        body.extend_from_slice(&fields);

        let mut stream = TokenStream::new(&body);
        match stream.next_token().unwrap().unwrap() {
            Token::LoginAck(ack) => {
                assert_eq!(ack.program, "mssq");
                assert_eq!(ack.version, (16, 0, 0x0FA0));
                assert_eq!(ack.tds_version, TDS_VERSION_7_4);
            }
            other => panic!("expected a login ack, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_token_stops_the_stream_rather_than_guessing() {
        let error = TokenStream::new(&[0x42]).next_token().unwrap_err().to_string();
        assert!(error.contains("0x42"), "{error}");
    }

    #[test]
    fn an_empty_message_yields_no_tokens() {
        assert!(TokenStream::new(&[]).next_token().unwrap().is_none());
    }

    #[test]
    fn a_truncated_token_is_a_protocol_error_not_a_panic() {
        // A DONE token that stops halfway through its row count.
        let mut body = vec![token::DONE];
        body.extend_from_slice(&[0, 0, 0, 0, 1, 2]);

        assert!(TokenStream::new(&body).next_token().is_err());
    }

    #[test]
    fn reads_little_endian_scalars_and_counted_strings() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x1234u16.to_le_bytes());
        bytes.extend_from_slice(&(-5i32).to_le_bytes());
        bytes.push(3);
        bytes.extend("ada".encode_utf16().flat_map(u16::to_le_bytes));

        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u16().unwrap(), 0x1234);
        assert_eq!(reader.i32().unwrap(), -5);
        assert_eq!(reader.b_varchar().unwrap(), "ada");
        assert!(reader.is_empty());
    }
}
