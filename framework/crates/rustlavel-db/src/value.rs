//! The values that cross the database boundary.
//!
//! One enum for everything a column can hold and everything a query can bind,
//! plus conversions so application code deals in Rust types rather than in
//! `Value` — `row.get::<i64>("id")`, not a match on a variant.

use rustlavel_core::{Error, Json, Result};
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    /// A `json`/`jsonb` column, kept parsed.
    Json(Json),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// How this value is written into SQL text for the wire.
    ///
    /// Parameters are sent out of band (never interpolated into the statement),
    /// so this is only the textual encoding of a bound parameter.
    pub fn to_sql_text(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Bool(true) => Some("t".into()),
            Value::Bool(false) => Some("f".into()),
            Value::Int(n) => Some(n.to_string()),
            Value::Float(n) => Some(n.to_string()),
            Value::Text(s) => Some(s.clone()),
            Value::Json(j) => Some(j.to_string()),
            // A bytea parameter goes over in the hex format Postgres expects.
            Value::Bytes(bytes) => {
                let mut out = String::with_capacity(2 + bytes.len() * 2);
                out.push_str("\\x");
                for byte in bytes {
                    out.push_str(&format!("{byte:02x}"));
                }
                Some(out)
            }
        }
    }

    /// Render for a human: log lines, Telescope, error messages.
    pub fn to_display(&self) -> String {
        match self {
            Value::Null => "NULL".into(),
            Value::Bytes(bytes) => format!("<{} bytes>", bytes.len()),
            other => other.to_sql_text().unwrap_or_default(),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_display())
    }
}

impl From<Value> for Json {
    fn from(value: Value) -> Json {
        match value {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(b),
            Value::Int(n) => Json::Number(n as f64),
            Value::Float(n) => Json::Number(n),
            Value::Text(s) => Json::String(s),
            Value::Json(j) => j,
            // Binary has no JSON representation; the length is the useful part.
            Value::Bytes(bytes) => Json::String(format!("<{} bytes>", bytes.len())),
        }
    }
}

macro_rules! from_int {
    ($($t:ty),*) => {
        $(impl From<$t> for Value {
            fn from(v: $t) -> Value {
                Value::Int(v as i64)
            }
        })*
    };
}
from_int!(i8, i16, i32, i64, u8, u16, u32, usize, isize);

impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Bool(v)
    }
}

impl From<f32> for Value {
    fn from(v: f32) -> Value {
        Value::Float(v as f64)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Value {
        Value::Float(v)
    }
}

impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::Text(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::Text(v.to_string())
    }
}

impl From<&String> for Value {
    fn from(v: &String) -> Value {
        Value::Text(v.clone())
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Value {
        Value::Bytes(v)
    }
}

impl From<Json> for Value {
    fn from(v: Json) -> Value {
        Value::Json(v)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map_or(Value::Null, Into::into)
    }
}

/// A Rust type a column can be read into.
///
/// The conversions are deliberately forgiving in one direction only: an `Int`
/// column reads into `f64`, but text never silently parses into a number, so a
/// schema mistake surfaces as an error instead of a wrong value.
pub trait FromValue: Sized {
    fn from_value(value: &Value) -> Result<Self>;
}

fn mismatch<T>(value: &Value) -> Result<T> {
    Err(Error::msg(format!(
        "cannot read a {} column as {}",
        variant_name(value),
        std::any::type_name::<T>()
    )))
}

fn variant_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Bool(_) => "boolean",
        Value::Int(_) => "integer",
        Value::Float(_) => "float",
        Value::Text(_) => "text",
        Value::Bytes(_) => "bytea",
        Value::Json(_) => "json",
    }
}

impl FromValue for Value {
    fn from_value(value: &Value) -> Result<Self> {
        Ok(value.clone())
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Text(s) => Ok(s.clone()),
            Value::Json(j) => Ok(j.to_string()),
            other => mismatch(other),
        }
    }
}

impl FromValue for i64 {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Int(n) => Ok(*n),
            other => mismatch(other),
        }
    }
}

impl FromValue for i32 {
    fn from_value(value: &Value) -> Result<Self> {
        i64::from_value(value).map(|n| n as i32)
    }
}

impl FromValue for f64 {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Float(n) => Ok(*n),
            Value::Int(n) => Ok(*n as f64),
            other => mismatch(other),
        }
    }
}

impl FromValue for bool {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Bool(b) => Ok(*b),
            // **MySQL has no boolean.** `BOOLEAN` is an alias for
            // `tinyint(1)`, and nothing on the wire tells a flag from a small
            // counter — so the driver cannot know, and hands back an integer.
            // The model does know: it declared the field `bool`. This is the
            // layer where that information exists, and resolving it here is
            // what keeps a `#[derive(Model)]` with a flag on it readable on
            // all three databases rather than on one.
            Value::Int(0) => Ok(false),
            Value::Int(1) => Ok(true),
            // Anything else is a column being read as the wrong thing — a
            // counter declared `bool`, most likely — and guessing `true` for
            // it would hide that for as long as the value stayed non-zero.
            Value::Int(n) => Err(Error::msg(format!(
                "cannot read {n} as a bool. A `bool` field maps to 0 or 1; this column holds \
                 something else, so it is probably a number rather than a flag."
            ))),
            other => mismatch(other),
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Bytes(bytes) => Ok(bytes.clone()),
            Value::Text(s) => Ok(s.clone().into_bytes()),
            other => mismatch(other),
        }
    }
}

impl FromValue for Json {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Json(j) => Ok(j.clone()),
            Value::Text(s) => Json::parse(s),
            other => Ok(other.clone().into()),
        }
    }
}

/// A nullable column reads into `Option<T>`; every other type treats NULL as an
/// error, which is what makes a missing value impossible to ignore by accident.
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Result<Self> {
        match value {
            Value::Null => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_from_rust_types() {
        assert_eq!(Value::from(7i32), Value::Int(7));
        assert_eq!(Value::from("hi"), Value::Text("hi".into()));
        assert_eq!(Value::from(true), Value::Bool(true));
        assert_eq!(Value::from(Option::<i64>::None), Value::Null);
        assert_eq!(Value::from(Some(3i64)), Value::Int(3));
    }

    #[test]
    fn reads_into_rust_types() {
        assert_eq!(i64::from_value(&Value::Int(7)).unwrap(), 7);
        assert_eq!(f64::from_value(&Value::Int(7)).unwrap(), 7.0);
        assert_eq!(String::from_value(&Value::Text("a".into())).unwrap(), "a");
        assert_eq!(Option::<i64>::from_value(&Value::Null).unwrap(), None);
    }

    #[test]
    fn a_null_column_read_as_a_non_option_is_an_error() {
        let error = i64::from_value(&Value::Null).unwrap_err();
        assert!(error.to_string().contains("NULL"));
    }

    /// MySQL has no boolean: `BOOLEAN` is `tinyint(1)`, and the driver hands
    /// back an integer because nothing on the wire tells a flag from a small
    /// counter. A model that declared the field `bool` does know, so this is
    /// where the two are reconciled — without it, every model with a flag on
    /// it is readable on PostgreSQL and SQL Server and not on MySQL.
    #[test]
    fn a_mysql_tinyint_reads_into_a_bool_but_a_counter_does_not() {
        assert!(!bool::from_value(&Value::Int(0)).unwrap());
        assert!(bool::from_value(&Value::Int(1)).unwrap());
        assert!(bool::from_value(&Value::Bool(true)).unwrap());

        // Not "any non-zero is true": a counter column declared `bool` is a
        // mistake, and guessing would hide it for as long as it stayed
        // non-zero.
        let error = bool::from_value(&Value::Int(7)).unwrap_err().to_string();
        assert!(error.contains("cannot read 7 as a bool"), "{error}");
        assert!(error.contains("probably a number rather than a flag"), "{error}");

        // And text still never becomes a flag.
        assert!(bool::from_value(&Value::Text("true".into())).is_err());
    }

    #[test]
    fn text_never_silently_becomes_a_number() {
        assert!(i64::from_value(&Value::Text("7".into())).is_err());
    }

    #[test]
    fn encodes_parameters_as_postgres_text() {
        assert_eq!(Value::Bool(true).to_sql_text().as_deref(), Some("t"));
        assert_eq!(Value::Null.to_sql_text(), None);
        assert_eq!(Value::Bytes(vec![0xde, 0xad]).to_sql_text().as_deref(), Some("\\xdead"));
    }
}
