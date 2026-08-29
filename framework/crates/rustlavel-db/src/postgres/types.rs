//! Decoding column text into [`Value`], guided by the column's type OID.

use crate::value::Value;
use rustlavel_core::Json;

// The OIDs the driver recognises. They are stable constants in PostgreSQL's
// catalog, which is why hard-coding them is safe.
pub const BOOL: i32 = 16;
pub const BYTEA: i32 = 17;
pub const CHAR: i32 = 18;
pub const NAME: i32 = 19;
pub const INT8: i32 = 20;
pub const INT2: i32 = 21;
pub const INT4: i32 = 23;
pub const TEXT: i32 = 25;
pub const OID: i32 = 26;
pub const JSON: i32 = 114;
pub const FLOAT4: i32 = 700;
pub const FLOAT8: i32 = 701;
pub const BPCHAR: i32 = 1042;
pub const VARCHAR: i32 = 1043;
pub const DATE: i32 = 1082;
pub const TIME: i32 = 1083;
pub const TIMESTAMP: i32 = 1114;
pub const TIMESTAMPTZ: i32 = 1184;
pub const NUMERIC: i32 = 1700;
pub const UUID: i32 = 2950;
pub const JSONB: i32 = 3802;

/// Turn a column's text representation into a [`Value`].
///
/// Timestamps, dates, numerics and uuids stay as text: the framework has no
/// date type of its own yet, and silently converting a `numeric` to `f64` would
/// lose the precision the column exists to preserve.
pub fn decode(type_oid: i32, raw: Option<&[u8]>) -> Value {
    let Some(bytes) = raw else { return Value::Null };
    let text = String::from_utf8_lossy(bytes);

    match type_oid {
        BOOL => Value::Bool(text == "t"),
        INT2 | INT4 | INT8 | OID => text.parse::<i64>().map_or(Value::Text(text.into_owned()), Value::Int),
        FLOAT4 | FLOAT8 => {
            text.parse::<f64>().map_or(Value::Text(text.into_owned()), Value::Float)
        }
        JSON | JSONB => Json::parse(&text).map_or(Value::Text(text.into_owned()), Value::Json),
        BYTEA => Value::Bytes(decode_bytea(&text)),
        _ => Value::Text(text.into_owned()),
    }
}

/// PostgreSQL sends `bytea` in text mode as `\xdeadbeef`.
fn decode_bytea(text: &str) -> Vec<u8> {
    let Some(hex) = text.strip_prefix("\\x") else {
        return text.as_bytes().to_vec();
    };

    hex.as_bytes()
        .chunks(2)
        .filter_map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}

/// The SQL type a schema builder emits for a logical column type.
pub fn type_name(oid: i32) -> &'static str {
    match oid {
        BOOL => "boolean",
        INT2 => "smallint",
        INT4 => "integer",
        INT8 => "bigint",
        FLOAT4 => "real",
        FLOAT8 => "double precision",
        NUMERIC => "numeric",
        TEXT => "text",
        VARCHAR | BPCHAR | CHAR | NAME => "varchar",
        DATE => "date",
        TIME => "time",
        TIMESTAMP => "timestamp",
        TIMESTAMPTZ => "timestamptz",
        UUID => "uuid",
        JSON => "json",
        JSONB => "jsonb",
        BYTEA => "bytea",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_scalar_types() {
        assert_eq!(decode(BOOL, Some(b"t")), Value::Bool(true));
        assert_eq!(decode(BOOL, Some(b"f")), Value::Bool(false));
        assert_eq!(decode(INT4, Some(b"42")), Value::Int(42));
        assert_eq!(decode(INT8, Some(b"-7")), Value::Int(-7));
        assert_eq!(decode(FLOAT8, Some(b"1.5")), Value::Float(1.5));
        assert_eq!(decode(TEXT, Some(b"hello")), Value::Text("hello".into()));
    }

    #[test]
    fn a_null_column_decodes_to_null_whatever_its_type() {
        assert_eq!(decode(INT4, None), Value::Null);
        assert_eq!(decode(TEXT, None), Value::Null);
    }

    #[test]
    fn decodes_json_columns_into_parsed_values() {
        let value = decode(JSONB, Some(br#"{"a":1}"#));
        match value {
            Value::Json(json) => assert_eq!(json.get("a").unwrap().as_i64(), Some(1)),
            other => panic!("expected parsed JSON, got {other:?}"),
        }
    }

    #[test]
    fn decodes_hex_bytea() {
        assert_eq!(decode(BYTEA, Some(b"\\xdead")), Value::Bytes(vec![0xde, 0xad]));
    }

    #[test]
    fn numerics_and_timestamps_stay_text_so_precision_survives() {
        assert_eq!(
            decode(NUMERIC, Some(b"12345.678901234567890")),
            Value::Text("12345.678901234567890".into())
        );
        assert_eq!(
            decode(TIMESTAMPTZ, Some(b"2026-08-29 10:00:00+00")),
            Value::Text("2026-08-29 10:00:00+00".into())
        );
    }
}
