//! TDS types: reading COLMETADATA and rows into [`Value`], and encoding bound
//! parameters on the way out.
//!
//! SQL Server sends everything in binary, so unlike the PostgreSQL driver —
//! which asks for text and parses it — this module owns a decoder per type.
//! Two decisions are worth stating up front, because they look like omissions:
//!
//! * `decimal`, `numeric` and `money` come back as [`Value::Text`]. A `decimal`
//!   is an exact type; turning it into `f64` would quietly lose the precision
//!   the column exists to preserve, and the framework has no decimal type yet.
//! * `date`, `time`, `datetime`, `datetime2` and `datetimeoffset` come back as
//!   text too, in ISO 8601 order. The framework has no date type yet, and the
//!   PostgreSQL driver already keeps timestamps as text, so a model that reads
//!   a timestamp as `String` behaves the same on both databases.
//!
//! `sql_variant` is the one type this module cannot promise: it is decoded when
//! its base type is one of the common scalars and returned as NULL otherwise,
//! having consumed exactly the right number of bytes so the token stream stays
//! aligned.

use super::protocol::{Reader, decode_ucs2};
use crate::dialect::{Dialect, SqlServer};
use crate::value::Value;
use rustlavel_core::{Error, Json, Result};

// Fixed-length types: the length is implied by the type byte.
pub const NULLTYPE: u8 = 0x1F;
pub const INT1TYPE: u8 = 0x30;
pub const BITTYPE: u8 = 0x32;
pub const INT2TYPE: u8 = 0x34;
pub const INT4TYPE: u8 = 0x38;
pub const DATETIM4TYPE: u8 = 0x3A;
pub const FLT4TYPE: u8 = 0x3B;
pub const MONEYTYPE: u8 = 0x3C;
pub const DATETIMETYPE: u8 = 0x3D;
pub const FLT8TYPE: u8 = 0x3E;
pub const MONEY4TYPE: u8 = 0x7A;
pub const INT8TYPE: u8 = 0x7F;

// Nullable and variable types with a one-byte length.
pub const GUIDTYPE: u8 = 0x24;
pub const INTNTYPE: u8 = 0x26;
pub const DECIMALTYPE: u8 = 0x37;
pub const NUMERICTYPE: u8 = 0x3F;
pub const BITNTYPE: u8 = 0x68;
pub const DECIMALNTYPE: u8 = 0x6A;
pub const NUMERICNTYPE: u8 = 0x6C;
pub const FLTNTYPE: u8 = 0x6D;
pub const MONEYNTYPE: u8 = 0x6E;
pub const DATETIMNTYPE: u8 = 0x6F;
pub const DATENTYPE: u8 = 0x28;
pub const TIMENTYPE: u8 = 0x29;
pub const DATETIME2NTYPE: u8 = 0x2A;
pub const DATETIMEOFFSETNTYPE: u8 = 0x2B;
pub const CHARTYPE: u8 = 0x2F;
pub const VARCHARTYPE: u8 = 0x27;
pub const BINARYTYPE: u8 = 0x2D;
pub const VARBINARYTYPE: u8 = 0x25;

// Types with a two-byte length. `BIG` is historical: they are the ones that can
// exceed 255 bytes, and the `(max)` forms are these with a length of 0xFFFF.
pub const BIGVARBINARYTYPE: u8 = 0xA5;
pub const BIGVARCHARTYPE: u8 = 0xA7;
pub const BIGBINARYTYPE: u8 = 0xAD;
pub const BIGCHARTYPE: u8 = 0xAF;
pub const NVARCHARTYPE: u8 = 0xE7;
pub const NCHARTYPE: u8 = 0xEF;

// Types with a four-byte length: the deprecated large-object types.
pub const IMAGETYPE: u8 = 0x22;
pub const TEXTTYPE: u8 = 0x23;
pub const NTEXTTYPE: u8 = 0x63;
pub const SSVARIANTTYPE: u8 = 0x62;
pub const XMLTYPE: u8 = 0xF1;

/// The marker a two-byte length carries when a column is declared `(max)`.
const MAX_LENGTH: usize = 0xFFFF;

/// How the value that follows a TYPE_INFO announces its own length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthStyle {
    /// The type byte implies the size and the value can never be NULL.
    Fixed,
    /// One length byte; zero means NULL.
    Byte,
    /// Two length bytes; 0xFFFF means NULL.
    Short,
    /// A text pointer, then four length bytes. `text`, `ntext` and `image`.
    Long,
    /// Partially length-prefixed: a total length then a run of chunks. This is
    /// how every `(max)` column arrives, and it is the only shape whose length
    /// may genuinely be unknown when the first byte is sent.
    Chunked,
}

/// A column's declared type, as COLMETADATA describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeInfo {
    pub kind: u8,
    /// The declared maximum size in bytes, or [`MAX_LENGTH`] for `(max)`.
    pub size: usize,
    pub precision: u8,
    pub scale: u8,
    pub collation: Option<Collation>,
    pub length_style: LengthStyle,
}

impl TypeInfo {
    fn fixed(kind: u8, size: usize) -> TypeInfo {
        TypeInfo {
            kind,
            size,
            precision: 0,
            scale: 0,
            collation: None,
            length_style: LengthStyle::Fixed,
        }
    }

    fn with_style(kind: u8, size: usize, length_style: LengthStyle) -> TypeInfo {
        TypeInfo { kind, size, precision: 0, scale: 0, collation: None, length_style }
    }
}

/// The five collation bytes that follow every character type.
///
/// Only one bit of it changes how the driver behaves — whether the column is
/// stored as UTF-8, which SQL Server 2019 introduced — but the LCID is kept so
/// a caller diagnosing a mojibake bug can see what the server claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collation {
    pub lcid: u32,
    pub flags: u32,
    pub sort_id: u8,
}

impl Collation {
    fn parse(bytes: &[u8]) -> Collation {
        let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Collation { lcid: word & 0x000F_FFFF, flags: word, sort_id: bytes[4] }
    }

    /// The fUTF8 bit, set by the `_UTF8` collations.
    pub fn is_utf8(&self) -> bool {
        self.flags & 0x0400_0000 != 0
    }
}

/// One column of a result set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub type_info: TypeInfo,
}

/// Read a COLMETADATA token body.
pub fn parse_column_metadata(reader: &mut Reader<'_>) -> Result<Vec<Column>> {
    let count = reader.u16()?;
    // 0xFFFF means "no metadata", which a statement returning nothing sends.
    if count == 0xFFFF {
        return Ok(Vec::new());
    }

    let mut columns = Vec::with_capacity(count as usize);
    for _ in 0..count {
        reader.u32()?; // user type
        reader.u16()?; // flags
        let type_info = parse_type_info(reader)?;

        // The large-object types carry the table they came from, which is only
        // read here to step over it.
        if matches!(type_info.kind, TEXTTYPE | NTEXTTYPE | IMAGETYPE) {
            let parts = reader.u8()?;
            for _ in 0..parts {
                reader.us_varchar()?;
            }
        }

        columns.push(Column { name: reader.b_varchar()?, type_info });
    }

    Ok(columns)
}

/// Read a TYPE_INFO: the type byte and whatever describes its size.
pub fn parse_type_info(reader: &mut Reader<'_>) -> Result<TypeInfo> {
    let kind = reader.u8()?;

    Ok(match kind {
        NULLTYPE => TypeInfo::fixed(kind, 0),
        INT1TYPE | BITTYPE => TypeInfo::fixed(kind, 1),
        INT2TYPE => TypeInfo::fixed(kind, 2),
        INT4TYPE | FLT4TYPE | MONEY4TYPE | DATETIM4TYPE => TypeInfo::fixed(kind, 4),
        INT8TYPE | FLT8TYPE | MONEYTYPE | DATETIMETYPE => TypeInfo::fixed(kind, 8),

        GUIDTYPE | INTNTYPE | BITNTYPE | FLTNTYPE | MONEYNTYPE | DATETIMNTYPE | CHARTYPE
        | VARCHARTYPE | BINARYTYPE | VARBINARYTYPE => {
            let size = reader.u8()? as usize;
            TypeInfo::with_style(kind, size, LengthStyle::Byte)
        }

        DECIMALTYPE | NUMERICTYPE | DECIMALNTYPE | NUMERICNTYPE => {
            let size = reader.u8()? as usize;
            TypeInfo {
                kind,
                size,
                precision: reader.u8()?,
                scale: reader.u8()?,
                collation: None,
                length_style: LengthStyle::Byte,
            }
        }

        // `date` is the one temporal type with no scale: it is always three
        // bytes of day count.
        DATENTYPE => TypeInfo::with_style(kind, 3, LengthStyle::Byte),

        TIMENTYPE | DATETIME2NTYPE | DATETIMEOFFSETNTYPE => TypeInfo {
            kind,
            size: 0,
            precision: 0,
            scale: reader.u8()?,
            collation: None,
            length_style: LengthStyle::Byte,
        },

        BIGVARBINARYTYPE | BIGBINARYTYPE => {
            let size = reader.u16()? as usize;
            TypeInfo::with_style(kind, size, length_for(size))
        }

        BIGVARCHARTYPE | BIGCHARTYPE | NVARCHARTYPE | NCHARTYPE => {
            let size = reader.u16()? as usize;
            let collation = Collation::parse(reader.take(5)?);
            TypeInfo {
                kind,
                size,
                precision: 0,
                scale: 0,
                collation: Some(collation),
                length_style: length_for(size),
            }
        }

        TEXTTYPE | NTEXTTYPE => {
            let size = reader.u32()? as usize;
            let collation = Collation::parse(reader.take(5)?);
            TypeInfo {
                kind,
                size,
                precision: 0,
                scale: 0,
                collation: Some(collation),
                length_style: LengthStyle::Long,
            }
        }

        IMAGETYPE => {
            let size = reader.u32()? as usize;
            TypeInfo::with_style(kind, size, LengthStyle::Long)
        }

        SSVARIANTTYPE => {
            let size = reader.u32()? as usize;
            TypeInfo::with_style(kind, size, LengthStyle::Long)
        }

        XMLTYPE => {
            // A schema collection may be named; it is read only to skip it.
            if reader.u8()? == 1 {
                reader.b_varchar()?; // database
                reader.b_varchar()?; // owning schema
                reader.us_varchar()?; // collection name
            }
            TypeInfo::with_style(kind, MAX_LENGTH, LengthStyle::Chunked)
        }

        other => {
            return Err(Error::Protocol(format!(
                "TDS type 0x{other:02X} is not one this driver decodes"
            )));
        }
    })
}

fn length_for(size: usize) -> LengthStyle {
    if size == MAX_LENGTH { LengthStyle::Chunked } else { LengthStyle::Short }
}

/// Read one ROW token: every column, in order.
pub fn read_row(reader: &mut Reader<'_>, columns: &[Column]) -> Result<Vec<Value>> {
    columns.iter().map(|column| read_value(reader, &column.type_info)).collect()
}

/// Read one NBCROW token — a "null bitmap compressed" row.
///
/// A leading bitmap marks which columns are NULL, and those columns then send
/// no bytes at all. It is pure bandwidth saving, and forgetting that the
/// omitted columns have no length prefix is the classic way to desynchronise a
/// TDS reader.
pub fn read_nbc_row(reader: &mut Reader<'_>, columns: &[Column]) -> Result<Vec<Value>> {
    let bitmap = reader.take(columns.len().div_ceil(8))?;

    let mut values = Vec::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        // Bits run least-significant-first within each byte.
        let is_null = bitmap[index / 8] & (1 << (index % 8)) != 0;
        values.push(if is_null {
            Value::Null
        } else {
            read_value(reader, &column.type_info)?
        });
    }

    Ok(values)
}

/// Read one value, given the type its column was declared with.
pub fn read_value(reader: &mut Reader<'_>, type_info: &TypeInfo) -> Result<Value> {
    let bytes = match type_info.length_style {
        LengthStyle::Fixed => {
            if type_info.size == 0 {
                return Ok(Value::Null);
            }
            reader.take(type_info.size)?.to_vec()
        }
        LengthStyle::Byte => match reader.u8()? {
            0 => return Ok(Value::Null),
            length => reader.take(length as usize)?.to_vec(),
        },
        LengthStyle::Short => match reader.u16()? {
            0xFFFF => return Ok(Value::Null),
            length => reader.take(length as usize)?.to_vec(),
        },
        LengthStyle::Long => match read_long(reader)? {
            None => return Ok(Value::Null),
            Some(bytes) => bytes,
        },
        LengthStyle::Chunked => match read_chunked(reader)? {
            None => return Ok(Value::Null),
            Some(bytes) => bytes,
        },
    };

    decode(type_info, &bytes)
}

/// The `text`/`ntext`/`image` shape: a text pointer, a timestamp, then a length.
fn read_long(reader: &mut Reader<'_>) -> Result<Option<Vec<u8>>> {
    let pointer = reader.u8()? as usize;
    if pointer == 0 {
        return Ok(None);
    }
    reader.skip(pointer)?;
    reader.skip(8)?; // the row's timestamp, which nothing here needs

    let length = reader.u32()? as usize;
    Ok(Some(reader.take(length)?.to_vec()))
}

/// The PLP shape every `(max)` column uses: a total length, then chunks, then a
/// zero-length chunk to finish.
///
/// The total may be 0xFFFFFFFFFFFFFFFE — "unknown" — which is why the chunks
/// are read until the terminator rather than until the total is reached.
fn read_chunked(reader: &mut Reader<'_>) -> Result<Option<Vec<u8>>> {
    const NULL: u64 = 0xFFFF_FFFF_FFFF_FFFF;
    const UNKNOWN: u64 = 0xFFFF_FFFF_FFFF_FFFE;

    let total = reader.u64()?;
    if total == NULL {
        return Ok(None);
    }

    let mut out = if total == UNKNOWN {
        Vec::new()
    } else {
        Vec::with_capacity(total.min(1 << 20) as usize)
    };

    loop {
        // A zero-length total is written with no chunks at all by some servers,
        // so an exhausted reader ends the value just as the terminator does.
        if total == 0 && reader.remaining() < 4 {
            return Ok(Some(out));
        }
        let length = reader.u32()? as usize;
        if length == 0 {
            return Ok(Some(out));
        }
        out.extend_from_slice(reader.take(length)?);
    }
}

/// Turn the raw bytes of one value into a [`Value`].
fn decode(type_info: &TypeInfo, bytes: &[u8]) -> Result<Value> {
    Ok(match type_info.kind {
        BITTYPE | BITNTYPE => boolean(bytes.first().copied().unwrap_or(0)),

        INT1TYPE => Value::Int(bytes[0] as i64),
        INT2TYPE => Value::Int(i16::from_le_bytes(fixed(bytes)?) as i64),
        INT4TYPE => Value::Int(i32::from_le_bytes(fixed(bytes)?) as i64),
        INT8TYPE => Value::Int(i64::from_le_bytes(fixed(bytes)?)),
        // `intn` is any of the four widths; the length that arrived says which.
        INTNTYPE => match bytes.len() {
            1 => Value::Int(bytes[0] as i64),
            2 => Value::Int(i16::from_le_bytes(fixed(bytes)?) as i64),
            4 => Value::Int(i32::from_le_bytes(fixed(bytes)?) as i64),
            8 => Value::Int(i64::from_le_bytes(fixed(bytes)?)),
            other => return Err(width("int", other)),
        },

        FLT4TYPE => Value::Float(f32::from_le_bytes(fixed(bytes)?) as f64),
        FLT8TYPE => Value::Float(f64::from_le_bytes(fixed(bytes)?)),
        FLTNTYPE => match bytes.len() {
            4 => Value::Float(f32::from_le_bytes(fixed(bytes)?) as f64),
            8 => Value::Float(f64::from_le_bytes(fixed(bytes)?)),
            other => return Err(width("float", other)),
        },

        // `money` is a fixed four-decimal integer. Kept as text for the same
        // reason as `decimal`: it is exact, and `f64` is not.
        MONEYTYPE | MONEY4TYPE | MONEYNTYPE => Value::Text(money(bytes)?),

        DECIMALTYPE | NUMERICTYPE | DECIMALNTYPE | NUMERICNTYPE => {
            Value::Text(decimal(bytes, type_info.scale)?)
        }

        GUIDTYPE => Value::Text(uuid(bytes)?),

        CHARTYPE | VARCHARTYPE | BIGCHARTYPE | BIGVARCHARTYPE | TEXTTYPE => {
            Value::Text(decode_char(type_info.collation, bytes))
        }

        NCHARTYPE | NVARCHARTYPE | NTEXTTYPE | XMLTYPE => Value::Text(decode_ucs2(bytes)),

        BINARYTYPE | VARBINARYTYPE | BIGBINARYTYPE | BIGVARBINARYTYPE | IMAGETYPE => {
            Value::Bytes(bytes.to_vec())
        }

        DATENTYPE => Value::Text(date_text(days(bytes))),
        TIMENTYPE => Value::Text(time_text(time_units(bytes), type_info.scale)),
        DATETIME2NTYPE => Value::Text(datetime2_text(bytes, type_info.scale)?),
        DATETIMEOFFSETNTYPE => Value::Text(datetimeoffset_text(bytes, type_info.scale)?),
        DATETIMETYPE | DATETIM4TYPE | DATETIMNTYPE => Value::Text(legacy_datetime_text(bytes)?),

        SSVARIANTTYPE => variant(bytes)?,

        NULLTYPE => Value::Null,

        other => {
            return Err(Error::Protocol(format!(
                "TDS type 0x{other:02X} arrived with no decoder"
            )));
        }
    })
}

/// SQL Server has no boolean: `bit` is what the dialect maps `Boolean` onto,
/// and [`Dialect::booleans_are_integers`] is where that fact is recorded. The
/// wire hands over a byte; this is the one place that turns it back into the
/// `Value::Bool` the rest of the framework expects, so a model field declared
/// `bool` reads identically on PostgreSQL and on SQL Server.
fn boolean(byte: u8) -> Value {
    if SqlServer.booleans_are_integers() {
        Value::Bool(byte != 0)
    } else {
        Value::Int(byte as i64)
    }
}

fn fixed<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes
        .get(..N)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| Error::Protocol(format!("expected {N} bytes for a fixed-width value")))
}

fn width(name: &str, got: usize) -> Error {
    Error::Protocol(format!("a {name} column arrived with an impossible width of {got} bytes"))
}

/// `money` is stored as an i64 of ten-thousandths, with its two halves swapped.
fn money(bytes: &[u8]) -> Result<String> {
    let units: i64 = match bytes.len() {
        4 => i32::from_le_bytes(fixed(bytes)?) as i64,
        8 => {
            let high = i32::from_le_bytes(fixed::<4>(&bytes[..4])?) as i64;
            let low = u32::from_le_bytes(fixed::<4>(&bytes[4..])?) as i64;
            (high << 32) | low
        }
        other => return Err(width("money", other)),
    };

    Ok(scaled(units as i128, 4))
}

/// `decimal` and `numeric`: a sign byte, then an unsigned little-endian
/// magnitude of 4, 8, 12 or 16 bytes, read against the column's scale.
fn decimal(bytes: &[u8], scale: u8) -> Result<String> {
    if bytes.is_empty() {
        return Err(Error::Protocol("a decimal value arrived with no sign byte".into()));
    }

    let positive = bytes[0] == 1;
    let mut magnitude: i128 = 0;
    for (index, byte) in bytes[1..].iter().enumerate() {
        if index >= 16 {
            return Err(width("decimal", bytes.len()));
        }
        magnitude |= (*byte as i128) << (index * 8);
    }

    Ok(scaled(if positive { magnitude } else { -magnitude }, scale))
}

/// Render an integer count of 10^-scale units as a decimal string.
fn scaled(units: i128, scale: u8) -> String {
    if scale == 0 {
        return units.to_string();
    }

    let divisor = 10i128.pow(scale as u32);
    let sign = if units < 0 { "-" } else { "" };
    let magnitude = units.unsigned_abs();

    format!(
        "{sign}{}.{:0width$}",
        magnitude / divisor as u128,
        magnitude % divisor as u128,
        width = scale as usize
    )
}

/// `uniqueidentifier` is a Microsoft GUID: the first three groups are stored
/// little-endian and the last two big-endian, which is why the bytes cannot
/// simply be printed in order.
fn uuid(bytes: &[u8]) -> Result<String> {
    if bytes.len() != 16 {
        return Err(width("uniqueidentifier", bytes.len()));
    }

    Ok(format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{}",
        bytes[3],
        bytes[2],
        bytes[1],
        bytes[0],
        bytes[5],
        bytes[4],
        bytes[7],
        bytes[6],
        bytes[8],
        bytes[9],
        bytes[10..].iter().map(|b| format!("{b:02X}")).collect::<String>()
    ))
}

/// Decode a non-Unicode character column.
///
/// The bytes are in whatever code page the collation names, and the framework
/// speaks UTF-8. A `_UTF8` collation is already UTF-8; anything else is decoded
/// as Windows-1252, which is the code page behind every default SQL Server
/// collation and is a superset of Latin-1. Well-formed UTF-8 is preferred
/// whatever the collation says, because a UTF-8 column read through a legacy
/// collation is the more common mistake of the two.
fn decode_char(collation: Option<Collation>, bytes: &[u8]) -> String {
    if collation.is_some_and(|c| c.is_utf8()) {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|byte| windows_1252(*byte)).collect(),
    }
}

/// The 0x80–0x9F range is where Windows-1252 differs from Latin-1; everything
/// else maps straight onto the code point of the same number.
fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];

    match byte {
        0x80..=0x9F => HIGH[(byte - 0x80) as usize],
        other => other as char,
    }
}

// --- Dates and times ---

/// The number of days from 0001-01-01, where SQL Server's `date` starts, to
/// 1970-01-01, where the civil-date arithmetic below starts.
const DAYS_0001_TO_1970: i64 = 719_162;

/// The number of days from 1900-01-01, where the legacy `datetime` starts.
const DAYS_1900_TO_1970: i64 = 25_567;

fn days(bytes: &[u8]) -> i64 {
    let mut total = 0i64;
    for (index, byte) in bytes.iter().take(3).enumerate() {
        total |= (*byte as i64) << (index * 8);
    }
    total
}

/// The count of 10^-scale second increments since midnight, from 3, 4 or 5 bytes.
fn time_units(bytes: &[u8]) -> u64 {
    let mut total = 0u64;
    for (index, byte) in bytes.iter().enumerate() {
        total |= (*byte as u64) << (index * 8);
    }
    total
}

/// Howard Hinnant's civil-from-days, which is exact for the proleptic
/// Gregorian calendar SQL Server uses and needs no lookup tables.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 { month_prime + 3 } else { month_prime - 9 } as u32;

    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn date_text(day_count: i64) -> String {
    let (year, month, day) = civil_from_days(day_count - DAYS_0001_TO_1970);
    format!("{year:04}-{month:02}-{day:02}")
}

fn time_text(units: u64, scale: u8) -> String {
    let per_second = 10u64.pow(scale as u32);
    let seconds = units / per_second;
    let fraction = units % per_second;

    let base = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    );

    if scale == 0 {
        base
    } else {
        format!("{base}.{fraction:0width$}", width = scale as usize)
    }
}

/// The number of bytes a `time` of a given scale occupies.
fn time_width(scale: u8) -> usize {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

fn datetime2_text(bytes: &[u8], scale: u8) -> Result<String> {
    let split = time_width(scale);
    if bytes.len() < split + 3 {
        return Err(width("datetime2", bytes.len()));
    }

    Ok(format!(
        "{} {}",
        date_text(days(&bytes[split..])),
        time_text(time_units(&bytes[..split]), scale)
    ))
}

fn datetimeoffset_text(bytes: &[u8], scale: u8) -> Result<String> {
    let time_len = time_width(scale);
    let split = time_len + 3;
    if bytes.len() < split + 2 {
        return Err(width("datetimeoffset", bytes.len()));
    }

    // The date and time on the wire are UTC; the trailing offset says which
    // local time they were written as. SQL Server renders the *local* time, so
    // the offset is applied before formatting — and applying it can cross
    // midnight, which is why the day count moves with it.
    let offset = i16::from_le_bytes([bytes[split], bytes[split + 1]]) as i64;
    let per_second = 10i64.pow(scale as u32);
    let day = per_second * 86_400;

    let shifted = time_units(&bytes[..time_len]) as i64 + offset * 60 * per_second;
    let local_day = days(&bytes[time_len..split]) + shifted.div_euclid(day);
    let local_time = shifted.rem_euclid(day) as u64;

    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs();

    Ok(format!(
        "{} {} {sign}{:02}:{:02}",
        date_text(local_day),
        time_text(local_time, scale),
        minutes / 60,
        minutes % 60
    ))
}

/// The two pre-2008 types: `datetime` counts three-hundredths of a second, and
/// `smalldatetime` counts whole minutes.
fn legacy_datetime_text(bytes: &[u8]) -> Result<String> {
    match bytes.len() {
        4 => {
            let day = u16::from_le_bytes(fixed::<2>(&bytes[..2])?) as i64;
            let minutes = u16::from_le_bytes(fixed::<2>(&bytes[2..])?) as u64;
            Ok(format!(
                "{} {}",
                date_text(day + DAYS_0001_TO_1970 - DAYS_1900_TO_1970),
                time_text(minutes * 60, 0)
            ))
        }
        8 => {
            let day = i32::from_le_bytes(fixed::<4>(&bytes[..4])?) as i64;
            let ticks = u32::from_le_bytes(fixed::<4>(&bytes[4..])?) as u64;
            // 300 ticks a second, rendered at millisecond precision the way
            // `select` renders it.
            let milliseconds = ticks * 1000 / 300;
            Ok(format!(
                "{} {}",
                date_text(day + DAYS_0001_TO_1970 - DAYS_1900_TO_1970),
                time_text(milliseconds, 3)
            ))
        }
        other => Err(width("datetime", other)),
    }
}

/// Decode a `sql_variant`: a base type, its property bytes, then the value.
///
/// Only the scalar base types are decoded. Anything else keeps the stream
/// aligned — the bytes have already been consumed by the caller — and comes
/// back as NULL rather than as a wrong value.
fn variant(bytes: &[u8]) -> Result<Value> {
    if bytes.len() < 2 {
        return Ok(Value::Null);
    }

    let kind = bytes[0];
    let properties = bytes[1] as usize;
    if bytes.len() < 2 + properties {
        return Ok(Value::Null);
    }
    let (properties, data) = (&bytes[2..2 + properties], &bytes[2 + properties..]);

    let type_info = match kind {
        BITTYPE | INT1TYPE | INT2TYPE | INT4TYPE | INT8TYPE | FLT4TYPE | FLT8TYPE | MONEYTYPE
        | MONEY4TYPE | GUIDTYPE | DATETIMETYPE | DATETIM4TYPE => {
            TypeInfo::fixed(kind, data.len())
        }
        DECIMALNTYPE | NUMERICNTYPE if properties.len() >= 2 => TypeInfo {
            kind,
            size: data.len(),
            precision: properties[0],
            scale: properties[1],
            collation: None,
            length_style: LengthStyle::Byte,
        },
        BIGVARCHARTYPE | BIGCHARTYPE if properties.len() >= 5 => TypeInfo {
            kind,
            size: data.len(),
            precision: 0,
            scale: 0,
            collation: Some(Collation::parse(properties)),
            length_style: LengthStyle::Short,
        },
        NVARCHARTYPE | NCHARTYPE => TypeInfo::with_style(kind, data.len(), LengthStyle::Short),
        BIGVARBINARYTYPE | BIGBINARYTYPE => {
            TypeInfo::with_style(kind, data.len(), LengthStyle::Short)
        }
        DATENTYPE => TypeInfo::with_style(kind, 3, LengthStyle::Byte),
        _ => return Ok(Value::Null),
    };

    decode(&type_info, data)
}

// --- Bound parameters ---

/// The type each [`Value`] is declared as in the `sp_executesql` parameter list.
///
/// Widest-of-its-kind on purpose: an `int` column accepts a `bigint` parameter,
/// but a `bigint` column would silently truncate an `int` one.
fn declared_type(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "bit",
        Value::Int(_) => "bigint",
        Value::Float(_) => "float",
        Value::Bytes(_) => "varbinary(max)",
        // A typed NULL has to be *some* type; `nvarchar` converts implicitly to
        // every other one, so it is the safe choice for a value with no type.
        Value::Null | Value::Text(_) | Value::Json(_) => "nvarchar(max)",
    }
}

/// The `@params` argument of `sp_executesql`: `@P1 bigint, @P2 nvarchar(max)`.
pub fn declare(params: &[Value]) -> String {
    params
        .iter()
        .enumerate()
        .map(|(index, value)| format!("@P{} {}", index + 1, declared_type(value)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Encode one bound parameter as TYPE_INFO followed by its value.
pub fn encode(value: &Value) -> Vec<u8> {
    match value {
        Value::Bool(flag) => vec![BITNTYPE, 1, 1, u8::from(*flag)],

        Value::Int(number) => {
            let mut out = vec![INTNTYPE, 8, 8];
            out.extend_from_slice(&number.to_le_bytes());
            out
        }

        Value::Float(number) => {
            let mut out = vec![FLTNTYPE, 8, 8];
            out.extend_from_slice(&number.to_le_bytes());
            out
        }

        Value::Bytes(bytes) => {
            let mut out = vec![BIGVARBINARYTYPE];
            out.extend_from_slice(&(MAX_LENGTH as u16).to_le_bytes());
            out.extend_from_slice(&chunked(Some(bytes)));
            out
        }

        Value::Null => nvarchar_max(None),
        Value::Text(text) => nvarchar_max(Some(&utf16(text))),
        Value::Json(json) => nvarchar_max(Some(&utf16(&json.to_string()))),
    }
}

fn nvarchar_max(bytes: Option<&[u8]>) -> Vec<u8> {
    let mut out = vec![NVARCHARTYPE];
    out.extend_from_slice(&(MAX_LENGTH as u16).to_le_bytes());
    // Five zero collation bytes: the server's own collation applies, which is
    // what an application means when it binds a string.
    out.extend_from_slice(&[0u8; 5]);
    out.extend_from_slice(&chunked(bytes));
    out
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Write a value in the PLP form every `(max)` parameter uses.
fn chunked(bytes: Option<&[u8]>) -> Vec<u8> {
    let Some(bytes) = bytes else {
        return 0xFFFF_FFFF_FFFF_FFFFu64.to_le_bytes().to_vec();
    };

    let mut out = Vec::with_capacity(bytes.len() + 16);
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    if !bytes.is_empty() {
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // terminator
    out
}

/// A JSON document read back out of an `nvarchar(max)` column.
///
/// SQL Server has no JSON type, so nothing on the wire says a column holds one.
/// This exists for callers that know it does.
pub fn as_json(value: &Value) -> Option<Json> {
    match value {
        Value::Text(text) => Json::parse(text).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a value the way the token stream does: a TYPE_INFO, then bytes.
    fn read(type_info_and_value: &[u8]) -> Value {
        let mut reader = Reader::new(type_info_and_value);
        let type_info = parse_type_info(&mut reader).expect("a type this driver knows");
        read_value(&mut reader, &type_info).expect("a decodable value")
    }

    #[test]
    fn decodes_every_width_of_integer() {
        assert_eq!(read(&[INTNTYPE, 1, 1, 7]), Value::Int(7));
        assert_eq!(read(&[INTNTYPE, 2, 2, 0xFF, 0xFF]), Value::Int(-1));
        assert_eq!(read(&[INTNTYPE, 4, 4, 0x2A, 0, 0, 0]), Value::Int(42));
        assert_eq!(
            read(&[INTNTYPE, 8, 8, 0x00, 0x1A, 0x71, 0x18, 0x02, 0, 0, 0]),
            Value::Int(9_000_000_000)
        );
        // A fixed-width int4 has no length byte at all.
        assert_eq!(read(&[INT4TYPE, 5, 0, 0, 0]), Value::Int(5));
    }

    #[test]
    fn a_bit_column_comes_back_as_a_boolean_because_the_dialect_says_so() {
        // SQL Server stores booleans as integers; the decoder converts them
        // back so `row.get::<bool>()` reads the same on every database.
        assert!(SqlServer.booleans_are_integers());
        assert_eq!(read(&[BITNTYPE, 1, 1, 1]), Value::Bool(true));
        assert_eq!(read(&[BITNTYPE, 1, 1, 0]), Value::Bool(false));
        assert_eq!(read(&[BITTYPE, 1]), Value::Bool(true));
    }

    #[test]
    fn decodes_floats_at_both_widths() {
        let mut single = vec![FLTNTYPE, 8, 4];
        single.extend_from_slice(&1.5f32.to_le_bytes());
        assert_eq!(read(&single), Value::Float(1.5));

        let mut double = vec![FLTNTYPE, 8, 8];
        double.extend_from_slice(&(-0.25f64).to_le_bytes());
        assert_eq!(read(&double), Value::Float(-0.25));
    }

    #[test]
    fn a_zero_length_value_is_null_whatever_its_type() {
        assert_eq!(read(&[INTNTYPE, 8, 0]), Value::Null);
        assert_eq!(read(&[BITNTYPE, 1, 0]), Value::Null);

        let mut nvarchar = vec![NVARCHARTYPE, 0x40, 0x00, 0, 0, 0, 0, 0];
        nvarchar.extend_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(read(&nvarchar), Value::Null);
    }

    #[test]
    fn decodes_an_nvarchar_and_the_max_variant_that_arrives_in_chunks() {
        let text: Vec<u8> = "héllo".encode_utf16().flat_map(u16::to_le_bytes).collect();

        let mut plain = vec![NVARCHARTYPE, 0x40, 0x00, 0, 0, 0, 0, 0];
        plain.extend_from_slice(&(text.len() as u16).to_le_bytes());
        plain.extend_from_slice(&text);
        assert_eq!(read(&plain), Value::Text("héllo".into()));

        // The same string as nvarchar(max), split across two chunks.
        let mut chunked = vec![NVARCHARTYPE, 0xFF, 0xFF, 0, 0, 0, 0, 0];
        chunked.extend_from_slice(&(text.len() as u64).to_le_bytes());
        chunked.extend_from_slice(&4u32.to_le_bytes());
        chunked.extend_from_slice(&text[..4]);
        chunked.extend_from_slice(&((text.len() - 4) as u32).to_le_bytes());
        chunked.extend_from_slice(&text[4..]);
        chunked.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(read(&chunked), Value::Text("héllo".into()));
    }

    #[test]
    fn a_varchar_is_decoded_against_its_collation() {
        // A Windows-1252 byte that is not valid UTF-8.
        let mut latin = vec![BIGVARCHARTYPE, 0x40, 0x00, 0x09, 0x04, 0xD0, 0x00, 0x34];
        latin.extend_from_slice(&2u16.to_le_bytes());
        latin.extend_from_slice(&[b'a', 0xE9]);
        assert_eq!(read(&latin), Value::Text("aé".into()));

        // The same bytes under a UTF-8 collation, where fUTF8 is set.
        let mut utf8 = vec![BIGVARCHARTYPE, 0x40, 0x00, 0x09, 0x04, 0x00, 0x04, 0x34];
        let bytes = "aé".as_bytes();
        utf8.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        utf8.extend_from_slice(bytes);
        assert_eq!(read(&utf8), Value::Text("aé".into()));
    }

    #[test]
    fn decodes_binary_including_the_max_variant() {
        let mut plain = vec![BIGVARBINARYTYPE, 0x10, 0x00];
        plain.extend_from_slice(&4u16.to_le_bytes());
        plain.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(read(&plain), Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));

        let mut max = vec![BIGVARBINARYTYPE, 0xFF, 0xFF];
        max.extend_from_slice(&2u64.to_le_bytes());
        max.extend_from_slice(&2u32.to_le_bytes());
        max.extend_from_slice(&[0x01, 0x02]);
        max.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(read(&max), Value::Bytes(vec![0x01, 0x02]));
    }

    #[test]
    fn decimals_stay_text_so_the_precision_they_exist_for_survives() {
        // 12345678901234567890 at scale 6 → 12345678901234.567890
        let mut bytes = vec![DECIMALNTYPE, 17, 38, 6, 17, 1];
        let mut magnitude = 12_345_678_901_234_567_890u128.to_le_bytes().to_vec();
        magnitude.truncate(16);
        bytes.extend_from_slice(&magnitude);
        assert_eq!(read(&bytes), Value::Text("12345678901234.567890".into()));

        // Negative, and with a scale that needs zero padding.
        let mut negative = vec![DECIMALNTYPE, 5, 10, 4, 5, 0];
        negative.extend_from_slice(&25u32.to_le_bytes());
        assert_eq!(read(&negative), Value::Text("-0.0025".into()));
    }

    #[test]
    fn money_keeps_its_four_decimal_places() {
        let mut bytes = vec![MONEYNTYPE, 8, 8];
        // 12.3400 → 123400 ten-thousandths, high half then low half.
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&123_400u32.to_le_bytes());
        assert_eq!(read(&bytes), Value::Text("12.3400".into()));
    }

    #[test]
    fn a_uniqueidentifier_swaps_the_first_three_groups() {
        let mut bytes = vec![GUIDTYPE, 16, 16];
        bytes.extend_from_slice(&[
            0x78, 0x56, 0x34, 0x12, 0x34, 0x12, 0x78, 0x56, 0x9A, 0xBC, 0xDE, 0xF0, 0x12, 0x34,
            0x56, 0x78,
        ]);

        assert_eq!(
            read(&bytes),
            Value::Text("12345678-1234-5678-9ABC-DEF012345678".into())
        );
    }

    #[test]
    fn dates_and_times_come_back_as_iso_text() {
        // 2026-08-29 is 739_856 days after 0001-01-01.
        let day = 739_856u32.to_le_bytes();
        assert_eq!(
            read(&[DATENTYPE, 3, day[0], day[1], day[2]]),
            Value::Text("2026-08-29".into())
        );

        // 10:30:00 at scale 7 is 10 * 3600 + 30 * 60 seconds of ten-millionths.
        let units = (37_800u64 * 10_000_000).to_le_bytes();
        let mut time = vec![TIMENTYPE, 7, 5];
        time.extend_from_slice(&units[..5]);
        assert_eq!(read(&time), Value::Text("10:30:00.0000000".into()));
    }

    #[test]
    fn a_datetime2_puts_the_time_before_the_date_on_the_wire() {
        let mut bytes = vec![DATETIME2NTYPE, 3, 7];
        // 01:02:03.004 at scale 3, then 2026-08-29.
        let units = (3_723_004u64).to_le_bytes();
        bytes.extend_from_slice(&units[..4]);
        bytes.extend_from_slice(&739_856u32.to_le_bytes()[..3]);

        assert_eq!(read(&bytes), Value::Text("2026-08-29 01:02:03.004".into()));
    }

    #[test]
    fn a_datetimeoffset_is_rendered_in_the_zone_it_was_written_in() {
        // The wire carries UTC — 06:32:03 — and an offset of −05:30, so the
        // local time the column was written as is 01:02:03.
        let mut bytes = vec![DATETIMEOFFSETNTYPE, 0, 8];
        bytes.extend_from_slice(&23_523u64.to_le_bytes()[..3]);
        bytes.extend_from_slice(&739_856u32.to_le_bytes()[..3]);
        bytes.extend_from_slice(&(-330i16).to_le_bytes());

        assert_eq!(read(&bytes), Value::Text("2026-08-29 01:02:03 -05:30".into()));
    }

    #[test]
    fn a_datetimeoffset_that_crosses_midnight_moves_the_date_with_it() {
        // 00:30:00 UTC at −05:30 is half past seven the previous evening.
        let mut bytes = vec![DATETIMEOFFSETNTYPE, 0, 8];
        bytes.extend_from_slice(&1_800u64.to_le_bytes()[..3]);
        bytes.extend_from_slice(&739_856u32.to_le_bytes()[..3]);
        bytes.extend_from_slice(&(-330i16).to_le_bytes());

        assert_eq!(read(&bytes), Value::Text("2026-08-28 19:00:00 -05:30".into()));
    }

    #[test]
    fn the_legacy_datetime_types_are_decoded_too() {
        // `datetime`: days since 1900-01-01 and three-hundredths of a second.
        let mut long = vec![DATETIMNTYPE, 8, 8];
        long.extend_from_slice(&46_261i32.to_le_bytes());
        long.extend_from_slice(&(300u32 * 3600).to_le_bytes());
        assert_eq!(read(&long), Value::Text("2026-08-29 01:00:00.000".into()));

        // `smalldatetime`: days and whole minutes.
        let mut short = vec![DATETIMNTYPE, 8, 4];
        short.extend_from_slice(&46_261u16.to_le_bytes());
        short.extend_from_slice(&90u16.to_le_bytes());
        assert_eq!(read(&short), Value::Text("2026-08-29 01:30:00".into()));
    }

    #[test]
    fn a_row_decodes_every_column_in_order() {
        let columns = vec![
            Column { name: "id".into(), type_info: TypeInfo::with_style(INTNTYPE, 8, LengthStyle::Byte) },
            Column {
                name: "flag".into(),
                type_info: TypeInfo::with_style(BITNTYPE, 1, LengthStyle::Byte),
            },
        ];

        let mut body = vec![8];
        body.extend_from_slice(&7i64.to_le_bytes());
        body.extend_from_slice(&[1, 1]);

        let values = read_row(&mut Reader::new(&body), &columns).unwrap();
        assert_eq!(values, vec![Value::Int(7), Value::Bool(true)]);
    }

    #[test]
    fn an_nbcrow_reads_its_null_bitmap_and_skips_the_columns_it_marks() {
        // Nine columns so the bitmap needs two bytes; the second and the ninth
        // are NULL and therefore send no bytes at all.
        let columns: Vec<Column> = (0..9)
            .map(|index| Column {
                name: format!("c{index}"),
                type_info: TypeInfo::with_style(INTNTYPE, 8, LengthStyle::Byte),
            })
            .collect();

        let mut body = vec![0b0000_0010, 0b0000_0001];
        for value in [0i64, 2, 3, 4, 5, 6, 7] {
            body.push(8);
            body.extend_from_slice(&value.to_le_bytes());
        }

        let values = read_nbc_row(&mut Reader::new(&body), &columns).unwrap();

        assert_eq!(values.len(), 9);
        assert_eq!(values[1], Value::Null);
        assert_eq!(values[8], Value::Null);
        assert_eq!(values[0], Value::Int(0));
        assert_eq!(values[7], Value::Int(7));
    }

    #[test]
    fn column_metadata_names_every_column_and_its_type() {
        let mut body = Vec::new();
        body.extend_from_slice(&2u16.to_le_bytes());

        body.extend_from_slice(&0u32.to_le_bytes()); // user type
        body.extend_from_slice(&0u16.to_le_bytes()); // flags
        body.extend_from_slice(&[INTNTYPE, 8]);
        body.push(2);
        body.extend("id".encode_utf16().flat_map(u16::to_le_bytes));

        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&[NVARCHARTYPE, 0xFF, 0xFF, 0, 0, 0, 0, 0]);
        body.push(4);
        body.extend("name".encode_utf16().flat_map(u16::to_le_bytes));

        let columns = parse_column_metadata(&mut Reader::new(&body)).unwrap();

        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[1].name, "name");
        // nvarchar(max) arrives in chunks, not with a plain length.
        assert_eq!(columns[1].type_info.length_style, LengthStyle::Chunked);
    }

    #[test]
    fn metadata_for_a_statement_that_returns_nothing_has_no_columns() {
        let body = 0xFFFFu16.to_le_bytes();
        assert!(parse_column_metadata(&mut Reader::new(&body)).unwrap().is_empty());
    }

    #[test]
    fn a_type_this_driver_cannot_decode_is_named_in_the_error() {
        let error = parse_type_info(&mut Reader::new(&[0xF0])).unwrap_err().to_string();
        assert!(error.contains("0xF0"), "{error}");
    }

    #[test]
    fn parameters_are_declared_with_the_widest_type_of_their_kind() {
        let declaration = declare(&[
            Value::Int(1),
            Value::Text("a".into()),
            Value::Bool(true),
            Value::Float(1.0),
            Value::Bytes(vec![1]),
            Value::Null,
        ]);

        assert_eq!(
            declaration,
            "@P1 bigint, @P2 nvarchar(max), @P3 bit, @P4 float, \
             @P5 varbinary(max), @P6 nvarchar(max)"
        );
        assert_eq!(declare(&[]), "");
    }

    #[test]
    fn an_encoded_parameter_round_trips_back_through_the_decoder() {
        // Whatever the encoder writes, the decoder must read: that is what
        // proves a bound value survives the trip unchanged.
        for value in [
            Value::Int(-9_000_000_000),
            Value::Bool(true),
            Value::Bool(false),
            Value::Float(1.5),
            Value::Text("'; drop table users; --".into()),
            Value::Text(String::new()),
            Value::Bytes(vec![0xDE, 0xAD]),
            Value::Null,
        ] {
            let encoded = encode(&value);
            let mut reader = Reader::new(&encoded);
            let type_info = parse_type_info(&mut reader).unwrap();
            let decoded = read_value(&mut reader, &type_info).unwrap();

            assert_eq!(decoded, value, "{value:?} did not survive encoding");
            assert!(reader.is_empty(), "{value:?} left bytes behind");
        }
    }

    #[test]
    fn a_null_parameter_is_sent_as_a_typed_null_not_as_an_empty_string() {
        let encoded = encode(&Value::Null);

        assert_eq!(encoded[0], NVARCHARTYPE);
        // The PLP length is the all-ones NULL marker, and no chunks follow.
        assert_eq!(&encoded[encoded.len() - 8..], &[0xFF; 8]);
    }

    #[test]
    fn json_is_bound_as_the_nvarchar_sql_server_stores_it_in() {
        let json = Json::parse(r#"{"a":1}"#).unwrap();
        let encoded = encode(&Value::from(json.clone()));

        assert_eq!(encoded[0], NVARCHARTYPE);
        match read(&encoded) {
            Value::Text(text) => assert_eq!(as_json(&Value::Text(text)).unwrap(), json),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn civil_dates_are_exact_across_leap_years_and_centuries() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));

        // 2000 was a leap year and 1900 was not, which is the pair of cases a
        // hand-written calendar gets wrong.
        assert_eq!(date_text(DAYS_0001_TO_1970 + 11_016), "2000-02-29");
        assert_eq!(date_text(DAYS_0001_TO_1970 - 25_508), "1900-03-01");
        // Day zero of the `date` type, which nothing else in the driver reaches.
        assert_eq!(date_text(0), "0001-01-01");
    }
}
