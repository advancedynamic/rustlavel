//! Turning MySQL's column types into [`Value`], and bound parameters back.
//!
//! MySQL answers a `COM_QUERY` in the *text* protocol, where every column is an
//! ASCII string, and a `COM_STMT_EXECUTE` in the *binary* protocol, where every
//! column is packed to its own width. The driver uses both — text for DDL,
//! binary for anything with parameters — so both directions live here.

use crate::mysql::protocol::{Column, Reader};
use crate::value::Value;
use rustlavel_core::{Json, Result};

// The type bytes a `ColumnDefinition41` can carry. They are fixed by the wire
// protocol rather than by a catalogue, which is why hard-coding them is safe.
pub const DECIMAL: u8 = 0x00;
pub const TINY: u8 = 0x01;
pub const SHORT: u8 = 0x02;
pub const LONG: u8 = 0x03;
pub const FLOAT: u8 = 0x04;
pub const DOUBLE: u8 = 0x05;
pub const NULL: u8 = 0x06;
pub const TIMESTAMP: u8 = 0x07;
pub const LONGLONG: u8 = 0x08;
pub const INT24: u8 = 0x09;
pub const DATE: u8 = 0x0A;
pub const TIME: u8 = 0x0B;
pub const DATETIME: u8 = 0x0C;
pub const YEAR: u8 = 0x0D;
pub const VARCHAR: u8 = 0x0F;
pub const BIT: u8 = 0x10;
pub const JSON: u8 = 0xF5;
pub const NEWDECIMAL: u8 = 0xF6;
pub const ENUM: u8 = 0xF7;
pub const SET: u8 = 0xF8;
pub const TINY_BLOB: u8 = 0xF9;
pub const MEDIUM_BLOB: u8 = 0xFA;
pub const LONG_BLOB: u8 = 0xFB;
pub const BLOB: u8 = 0xFC;
pub const VAR_STRING: u8 = 0xFD;
pub const STRING: u8 = 0xFE;
pub const GEOMETRY: u8 = 0xFF;

/// Decode one column of a text-protocol row.
///
/// Note what does *not* happen here: a `tinyint(1)` comes back as an integer
/// and stays one. The MySQL dialect answers `booleans_are_integers()` with
/// `true` precisely because the wire cannot tell a `boolean` column from a
/// one-digit number, and a driver that guessed would turn a legitimate
/// `tinyint(1)` counter into `false` the moment it held zero.
///
/// DECIMAL, DATE, DATETIME and TIMESTAMP stay as text, exactly as the
/// PostgreSQL driver keeps NUMERIC and timestamps as text: `decimal` exists to
/// hold a value `f64` cannot, so converting would throw away the reason the
/// column was chosen, and the framework has no date type of its own yet, so
/// there is nothing better than the server's own rendering to convert into.
pub fn decode_text(column: &Column, raw: Option<&[u8]>) -> Value {
    let Some(bytes) = raw else { return Value::Null };

    match column.column_type {
        TINY | SHORT | LONG | INT24 | LONGLONG | YEAR => decode_integer_text(bytes, column),
        FLOAT | DOUBLE => {
            let text = String::from_utf8_lossy(bytes);
            text.parse::<f64>().map_or_else(|_| Value::Text(text.into_owned()), Value::Float)
        }
        JSON => {
            let text = String::from_utf8_lossy(bytes);
            Json::parse(&text).map_or_else(|_| Value::Text(text.into_owned()), Value::Json)
        }
        BIT => Value::Int(bits_to_int(bytes)),
        NULL => Value::Null,
        _ if column.is_binary() && is_string_type(column.column_type) => {
            Value::Bytes(bytes.to_vec())
        }
        _ => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// Decode one column of a binary-protocol row, advancing the reader past it.
///
/// The caller has already consulted the NULL bitmap: a NULL column occupies no
/// bytes at all here, so this is only ever asked about a present value.
pub fn decode_binary(column: &Column, reader: &mut Reader<'_>) -> Result<Value> {
    Ok(match column.column_type {
        NULL => Value::Null,
        TINY => {
            let byte = reader.u8()?;
            // Signedness is a column flag, not a separate type, so the same
            // byte means 255 or -1 depending on how the column was declared.
            if column.is_unsigned() { Value::Int(byte as i64) } else { Value::Int(byte as i8 as i64) }
        }
        SHORT | YEAR => {
            let value = reader.u16()?;
            if column.is_unsigned() { Value::Int(value as i64) } else { Value::Int(value as i16 as i64) }
        }
        LONG | INT24 => {
            let value = reader.u32()?;
            if column.is_unsigned() { Value::Int(value as i64) } else { Value::Int(value as i32 as i64) }
        }
        LONGLONG => {
            let value = reader.u64()?;
            // An unsigned bigint above i64::MAX has no home in `Value::Int`;
            // it becomes text rather than silently wrapping to a negative.
            if column.is_unsigned() && value > i64::MAX as u64 {
                Value::Text(value.to_string())
            } else {
                Value::Int(value as i64)
            }
        }
        FLOAT => Value::Float(f32::from_le_bytes(reader.take(4)?.try_into().expect("4 bytes")) as f64),
        DOUBLE => Value::Float(f64::from_le_bytes(reader.take(8)?.try_into().expect("8 bytes"))),
        DATE | DATETIME | TIMESTAMP => Value::Text(decode_binary_datetime(reader, column.column_type)?),
        TIME => Value::Text(decode_binary_time(reader)?),
        JSON => {
            let bytes = reader.lenenc_bytes()?;
            let text = String::from_utf8_lossy(bytes);
            Json::parse(&text).map_or_else(|_| Value::Text(text.into_owned()), Value::Json)
        }
        BIT => Value::Int(bits_to_int(reader.lenenc_bytes()?)),
        _ => {
            let bytes = reader.lenenc_bytes()?;
            if column.is_binary() {
                Value::Bytes(bytes.to_vec())
            } else {
                Value::Text(String::from_utf8_lossy(bytes).into_owned())
            }
        }
    })
}

/// The type byte and unsigned flag a bound parameter is sent with.
///
/// Deliberately coarse: every integer goes as `bigint` and every string as
/// `var_string`, and the server narrows them to the column's real type. Sending
/// the widest type that can hold the value means the driver never has to guess
/// what the statement will do with it.
pub fn bind_type(value: &Value) -> (u8, bool) {
    match value {
        Value::Null => (NULL, false),
        // MySQL has no boolean; `tinyint(1)` is what the dialect emits, and 1
        // and 0 are what the server compares against.
        Value::Bool(_) => (TINY, false),
        Value::Int(_) => (LONGLONG, false),
        Value::Float(_) => (DOUBLE, false),
        Value::Text(_) | Value::Json(_) => (VAR_STRING, false),
        Value::Bytes(_) => (BLOB, false),
    }
}

/// Append a bound parameter's binary form.
///
/// A NULL writes nothing: it is carried entirely by the NULL bitmap, which is
/// why this is a no-op rather than an error.
pub fn encode_bind(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => {}
        Value::Bool(flag) => out.push(u8::from(*flag)),
        Value::Int(number) => out.extend_from_slice(&number.to_le_bytes()),
        Value::Float(number) => out.extend_from_slice(&number.to_le_bytes()),
        Value::Text(text) => encode_lenenc(text.as_bytes(), out),
        Value::Json(json) => encode_lenenc(json.to_string().as_bytes(), out),
        Value::Bytes(bytes) => encode_lenenc(bytes, out),
    }
}

/// The name this column type has in SQL, for diagnostics.
pub fn type_name(column_type: u8) -> &'static str {
    match column_type {
        DECIMAL | NEWDECIMAL => "decimal",
        TINY => "tinyint",
        SHORT => "smallint",
        LONG => "int",
        FLOAT => "float",
        DOUBLE => "double",
        NULL => "null",
        TIMESTAMP => "timestamp",
        LONGLONG => "bigint",
        INT24 => "mediumint",
        DATE => "date",
        TIME => "time",
        DATETIME => "datetime",
        YEAR => "year",
        VARCHAR | VAR_STRING => "varchar",
        BIT => "bit",
        JSON => "json",
        ENUM => "enum",
        SET => "set",
        TINY_BLOB | MEDIUM_BLOB | LONG_BLOB | BLOB => "blob",
        STRING => "char",
        GEOMETRY => "geometry",
        _ => "unknown",
    }
}

fn is_string_type(column_type: u8) -> bool {
    matches!(
        column_type,
        VARCHAR | VAR_STRING | STRING | TINY_BLOB | MEDIUM_BLOB | LONG_BLOB | BLOB | GEOMETRY
    )
}

fn decode_integer_text(bytes: &[u8], column: &Column) -> Value {
    let text = String::from_utf8_lossy(bytes);

    // An unsigned bigint can exceed i64; it stays text rather than wrapping.
    if column.is_unsigned()
        && let Ok(large) = text.parse::<u64>()
    {
        return if large > i64::MAX as u64 {
            Value::Text(text.into_owned())
        } else {
            Value::Int(large as i64)
        };
    }

    text.parse::<i64>().map_or_else(|_| Value::Text(text.into_owned()), Value::Int)
}

/// A `bit` column arrives as big-endian bytes of whatever width it was declared.
fn bits_to_int(bytes: &[u8]) -> i64 {
    bytes.iter().fold(0i64, |accumulated, byte| (accumulated << 8) | *byte as i64)
}

/// `DATE`, `DATETIME` and `TIMESTAMP` in binary form: a length byte, then as
/// many of the fields as the value needs.
fn decode_binary_datetime(reader: &mut Reader<'_>, column_type: u8) -> Result<String> {
    let length = reader.u8()?;
    let date_only = column_type == DATE;

    if length == 0 {
        // The zero date. MySQL renders it this way too, rather than as an error.
        return Ok(if date_only { "0000-00-00".into() } else { "0000-00-00 00:00:00".into() });
    }

    let year = reader.u16()?;
    let month = reader.u8()?;
    let day = reader.u8()?;
    let date = format!("{year:04}-{month:02}-{day:02}");

    if length == 4 {
        return Ok(if date_only { date } else { format!("{date} 00:00:00") });
    }

    let hour = reader.u8()?;
    let minute = reader.u8()?;
    let second = reader.u8()?;
    let time = format!("{hour:02}:{minute:02}:{second:02}");

    if length == 7 {
        return Ok(format!("{date} {time}"));
    }

    let microseconds = reader.u32()?;
    Ok(format!("{date} {time}.{microseconds:06}"))
}

/// `TIME` in binary form. It is a duration, not a clock reading, so it can be
/// negative and can run past 24 hours — which is why the days field exists.
fn decode_binary_time(reader: &mut Reader<'_>) -> Result<String> {
    let length = reader.u8()?;
    if length == 0 {
        return Ok("00:00:00".into());
    }

    let negative = reader.u8()? == 1;
    let days = reader.u32()?;
    let hour = reader.u8()? as u32;
    let minute = reader.u8()?;
    let second = reader.u8()?;
    let sign = if negative { "-" } else { "" };
    let hours = days * 24 + hour;

    if length == 8 {
        return Ok(format!("{sign}{hours:02}:{minute:02}:{second:02}"));
    }

    let microseconds = reader.u32()?;
    Ok(format!("{sign}{hours:02}:{minute:02}:{second:02}.{microseconds:06}"))
}

fn encode_lenenc(bytes: &[u8], out: &mut Vec<u8>) {
    match bytes.len() as u64 {
        length @ 0..=0xFA => out.push(length as u8),
        length @ 0xFB..=0xFFFF => {
            out.push(0xFC);
            out.extend_from_slice(&(length as u16).to_le_bytes());
        }
        length @ 0x1_0000..=0xFF_FFFF => {
            out.push(0xFD);
            out.extend_from_slice(&(length as u32).to_le_bytes()[..3]);
        }
        length => {
            out.push(0xFE);
            out.extend_from_slice(&length.to_le_bytes());
        }
    }
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mysql::protocol::{CHARSET_BINARY, CHARSET_UTF8MB4, UNSIGNED_FLAG};

    fn column(column_type: u8) -> Column {
        Column { column_type, charset: CHARSET_UTF8MB4 as u16, ..Column::default() }
    }

    fn unsigned(column_type: u8) -> Column {
        Column { flags: UNSIGNED_FLAG, ..column(column_type) }
    }

    fn binary(column_type: u8) -> Column {
        Column { charset: CHARSET_BINARY, ..column(column_type) }
    }

    #[test]
    fn decodes_the_scalar_types_from_text() {
        assert_eq!(decode_text(&column(LONG), Some(b"42")), Value::Int(42));
        assert_eq!(decode_text(&column(LONGLONG), Some(b"-7")), Value::Int(-7));
        assert_eq!(decode_text(&column(DOUBLE), Some(b"1.5")), Value::Float(1.5));
        assert_eq!(
            decode_text(&column(VAR_STRING), Some(b"hello")),
            Value::Text("hello".into())
        );
    }

    #[test]
    fn a_null_column_decodes_to_null_whatever_its_type() {
        assert_eq!(decode_text(&column(LONG), None), Value::Null);
        assert_eq!(decode_text(&column(VAR_STRING), None), Value::Null);
    }

    #[test]
    fn a_tinyint_one_stays_an_integer_because_the_dialect_says_so() {
        // The dialect's `booleans_are_integers()` is true for MySQL: nothing on
        // the wire separates `boolean` from a one-digit number, so guessing
        // would break a genuine tinyint counter.
        use crate::dialect::{Dialect, MySql};
        assert!(MySql.booleans_are_integers());

        assert_eq!(decode_text(&column(TINY), Some(b"1")), Value::Int(1));
        assert_eq!(decode_text(&column(TINY), Some(b"0")), Value::Int(0));

        let mut reader = Reader::new(&[1]);
        assert_eq!(decode_binary(&column(TINY), &mut reader).unwrap(), Value::Int(1));
    }

    #[test]
    fn decimals_and_timestamps_stay_text_so_precision_survives() {
        assert_eq!(
            decode_text(&column(NEWDECIMAL), Some(b"12345.678901234567890")),
            Value::Text("12345.678901234567890".into())
        );
        assert_eq!(
            decode_text(&column(DATETIME), Some(b"2026-08-29 10:00:00.123456")),
            Value::Text("2026-08-29 10:00:00.123456".into())
        );
        assert_eq!(
            decode_text(&column(DATE), Some(b"2026-08-29")),
            Value::Text("2026-08-29".into())
        );
    }

    #[test]
    fn decodes_json_columns_into_parsed_values() {
        match decode_text(&column(JSON), Some(br#"{"a":1}"#)) {
            Value::Json(json) => assert_eq!(json.get("a").unwrap().as_i64(), Some(1)),
            other => panic!("expected parsed JSON, got {other:?}"),
        }
    }

    #[test]
    fn a_binary_collation_makes_a_string_column_bytes() {
        assert_eq!(
            decode_text(&binary(BLOB), Some(&[0xDE, 0xAD])),
            Value::Bytes(vec![0xDE, 0xAD])
        );
        // The same type byte with a text collation is text.
        assert_eq!(decode_text(&column(BLOB), Some(b"note")), Value::Text("note".into()));
    }

    #[test]
    fn signedness_comes_from_the_column_flag_not_the_type() {
        let mut reader = Reader::new(&[0xFF]);
        assert_eq!(decode_binary(&column(TINY), &mut reader).unwrap(), Value::Int(-1));

        let mut reader = Reader::new(&[0xFF]);
        assert_eq!(decode_binary(&unsigned(TINY), &mut reader).unwrap(), Value::Int(255));

        assert_eq!(decode_text(&unsigned(LONGLONG), Some(b"255")), Value::Int(255));
    }

    #[test]
    fn an_unsigned_bigint_too_large_for_i64_stays_text_rather_than_wrapping() {
        let huge = u64::MAX;
        assert_eq!(
            decode_text(&unsigned(LONGLONG), Some(huge.to_string().as_bytes())),
            Value::Text(huge.to_string())
        );

        let bytes = huge.to_le_bytes();
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            decode_binary(&unsigned(LONGLONG), &mut reader).unwrap(),
            Value::Text(huge.to_string())
        );
    }

    #[test]
    fn decodes_binary_integers_and_floats() {
        let mut reader = Reader::new(&9_000_000_000i64.to_le_bytes());
        assert_eq!(
            decode_binary(&column(LONGLONG), &mut reader).unwrap(),
            Value::Int(9_000_000_000)
        );

        let mut reader = Reader::new(&1.5f64.to_le_bytes());
        assert_eq!(decode_binary(&column(DOUBLE), &mut reader).unwrap(), Value::Float(1.5));

        let mut reader = Reader::new(&0.5f32.to_le_bytes());
        assert_eq!(decode_binary(&column(FLOAT), &mut reader).unwrap(), Value::Float(0.5));
    }

    #[test]
    fn decodes_a_binary_datetime_at_each_of_its_lengths() {
        // Length 0: the zero date.
        let mut reader = Reader::new(&[0]);
        assert_eq!(
            decode_binary(&column(DATETIME), &mut reader).unwrap(),
            Value::Text("0000-00-00 00:00:00".into())
        );

        // Length 4: a date, which a DATE column renders without a time.
        let date = [4u8, 0xEA, 0x07, 8, 29];
        let mut reader = Reader::new(&date);
        assert_eq!(
            decode_binary(&column(DATE), &mut reader).unwrap(),
            Value::Text("2026-08-29".into())
        );
        let mut reader = Reader::new(&date);
        assert_eq!(
            decode_binary(&column(DATETIME), &mut reader).unwrap(),
            Value::Text("2026-08-29 00:00:00".into())
        );

        // Length 7: to the second.
        let mut reader = Reader::new(&[7u8, 0xEA, 0x07, 8, 29, 10, 30, 5]);
        assert_eq!(
            decode_binary(&column(TIMESTAMP), &mut reader).unwrap(),
            Value::Text("2026-08-29 10:30:05".into())
        );

        // Length 11: with microseconds.
        let mut full = vec![11u8, 0xEA, 0x07, 8, 29, 10, 30, 5];
        full.extend_from_slice(&123_456u32.to_le_bytes());
        let mut reader = Reader::new(&full);
        assert_eq!(
            decode_binary(&column(DATETIME), &mut reader).unwrap(),
            Value::Text("2026-08-29 10:30:05.123456".into())
        );
    }

    #[test]
    fn a_binary_time_is_a_duration_so_it_can_be_negative_and_pass_a_day() {
        let mut payload = vec![8u8, 1];
        payload.extend_from_slice(&2u32.to_le_bytes()); // two days
        payload.extend_from_slice(&[3, 4, 5]);

        let mut reader = Reader::new(&payload);
        assert_eq!(
            decode_binary(&column(TIME), &mut reader).unwrap(),
            Value::Text("-51:04:05".into())
        );
    }

    #[test]
    fn a_bit_column_reads_as_the_number_its_bits_spell() {
        assert_eq!(decode_text(&column(BIT), Some(&[0x01, 0x00])), Value::Int(256));

        let mut reader = Reader::new(&[2, 0x01, 0x00]);
        assert_eq!(decode_binary(&column(BIT), &mut reader).unwrap(), Value::Int(256));
    }

    #[test]
    fn binds_each_value_as_the_widest_type_that_holds_it() {
        assert_eq!(bind_type(&Value::Null), (NULL, false));
        assert_eq!(bind_type(&Value::Bool(true)), (TINY, false));
        assert_eq!(bind_type(&Value::Int(1)), (LONGLONG, false));
        assert_eq!(bind_type(&Value::Float(1.0)), (DOUBLE, false));
        assert_eq!(bind_type(&Value::Text("a".into())), (VAR_STRING, false));
        assert_eq!(bind_type(&Value::Bytes(vec![1])), (BLOB, false));
    }

    #[test]
    fn encodes_bound_parameters_in_binary() {
        let mut out = Vec::new();
        encode_bind(&Value::Int(42), &mut out);
        assert_eq!(out, 42i64.to_le_bytes());

        let mut out = Vec::new();
        encode_bind(&Value::Text("ada".into()), &mut out);
        assert_eq!(out, b"\x03ada");

        let mut out = Vec::new();
        encode_bind(&Value::Bool(true), &mut out);
        assert_eq!(out, [1]);

        // A NULL is carried by the bitmap alone.
        let mut out = Vec::new();
        encode_bind(&Value::Null, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_bound_string_longer_than_a_byte_length_still_encodes() {
        let long = "x".repeat(300);
        let mut out = Vec::new();
        encode_bind(&Value::Text(long.clone()), &mut out);

        assert_eq!(out[0], 0xFC);
        assert_eq!(u16::from_le_bytes([out[1], out[2]]), 300);
        assert_eq!(&out[3..], long.as_bytes());
    }

    #[test]
    fn a_hostile_string_is_encoded_as_data_not_as_syntax() {
        // The bytes go over with a length in front of them; there is no
        // quoting step that could be got wrong.
        let hostile = "'; drop table users; --";
        let mut out = Vec::new();
        encode_bind(&Value::Text(hostile.into()), &mut out);

        assert_eq!(out[0] as usize, hostile.len());
        assert_eq!(&out[1..], hostile.as_bytes());
    }

    #[test]
    fn names_the_types_it_knows() {
        assert_eq!(type_name(LONGLONG), "bigint");
        assert_eq!(type_name(NEWDECIMAL), "decimal");
        assert_eq!(type_name(JSON), "json");
        assert_eq!(type_name(0x77), "unknown");
    }
}
