//! The vocabulary both signals share: attribute values, the resource that says
//! who is reporting, and the scope that says which library reported.
//!
//! Traces and metrics are different messages but they quote the same three
//! sub-messages, so these live once rather than once per signal — if the
//! attribute encoding drifted between the two, half the telemetry would arrive
//! unreadable and the other half would look fine.

use crate::protobuf::Encoder;
use rustlavel_core::Json;
use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

/// The scope name the collector shows against every span and metric this crate
/// produces, so telemetry from the framework is distinguishable from an
/// application's own.
pub const SCOPE_NAME: &str = "rustlavel-otel";
pub const SCOPE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One OTLP `AnyValue`, in the shapes telemetry actually uses.
///
/// Integers are kept apart from doubles on purpose. A status code arriving as
/// `200.0` is legal OTLP and still wrong: backends that index attributes by
/// type will file it beside latencies rather than beside other status codes.
#[derive(Debug, Clone)]
pub enum Value {
    String(String),
    Bool(bool),
    Int(i64),
    Double(f64),
    Array(Vec<Value>),
}

/// A set of attributes is a key in the meter's aggregation map, which needs a
/// total order. Floats have none by default, so this uses `f64::total_cmp` —
/// a NaN attribute is nonsense but it must not be allowed to make the map
/// inconsistent and start losing entries.
impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        fn rank(value: &Value) -> u8 {
            match value {
                Value::String(_) => 0,
                Value::Bool(_) => 1,
                Value::Int(_) => 2,
                Value::Double(_) => 3,
                Value::Array(_) => 4,
            }
        }

        match (self, other) {
            (Value::String(a), Value::String(b)) => a.cmp(b),
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Double(a), Value::Double(b)) => a.total_cmp(b),
            (Value::Array(a), Value::Array(b)) => a.cmp(b),
            (a, b) => rank(a).cmp(&rank(b)),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl Value {
    /// Adapt a value off the instrumentation bus.
    ///
    /// The bus carries [`Json`], whose only number is a double, so a whole
    /// number is narrowed back to an integer here. Row counts and token counts
    /// arrive that way and are meant to be integers on the far side.
    pub fn from_json(value: &Json) -> Value {
        match value {
            Json::String(text) => Value::String(text.clone()),
            Json::Bool(flag) => Value::Bool(*flag),
            Json::Number(number) => {
                if number.fract() == 0.0 && number.abs() < 9.007_199_254_740_992e15 {
                    Value::Int(*number as i64)
                } else {
                    Value::Double(*number)
                }
            }
            Json::Array(items) => Value::Array(items.iter().map(Value::from_json).collect()),
            // An object has no flat OTLP shape worth guessing at, and a nested
            // `kvlist` attribute is dropped by most backends anyway. Render it
            // so the information survives as text rather than vanishing.
            other => Value::String(other.to_string()),
        }
    }

    /// `AnyValue` is a `oneof`, so every arm is written even when it holds its
    /// type's default. Skipping `false`, `0` or `""` the way proto3 skips a
    /// plain scalar leaves the `oneof` unset, and the attribute arrives empty
    /// behind a 200 — which is how this was found, against a real collector.
    fn encode(&self) -> Encoder {
        let mut any = Encoder::new();
        match self {
            Value::String(text) => any.string_present(1, text),
            Value::Bool(flag) => any.bool_present(2, *flag),
            Value::Int(number) => any.int64_present(3, *number),
            Value::Double(number) => any.present_double(4, *number),
            Value::Array(items) => {
                let mut array = Encoder::new();
                for item in items {
                    array.message(1, &item.encode());
                }
                any.message(5, &array);
            }
        }
        any
    }

    /// The proto3 JSON mapping of `AnyValue`.
    ///
    /// `int64` becomes a *string*: JSON numbers are doubles, and a 64-bit
    /// integer past 2^53 would come back changed. The mapping requires it, and
    /// a collector that reads `intValue` as a number is being lenient.
    fn to_json(&self) -> Json {
        match self {
            Value::String(text) => Json::object([("stringValue", Json::from(text.clone()))]),
            Value::Bool(flag) => Json::object([("boolValue", Json::from(*flag))]),
            Value::Int(number) => Json::object([("intValue", Json::from(number.to_string()))]),
            Value::Double(number) => Json::object([("doubleValue", Json::Number(*number))]),
            Value::Array(items) => Json::object([(
                "arrayValue",
                Json::object([("values", Json::Array(items.iter().map(Value::to_json).collect()))]),
            )]),
        }
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::String(value.to_string())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::String(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Value::Int(value)
    }
}

impl From<u16> for Value {
    fn from(value: u16) -> Self {
        Value::Int(i64::from(value))
    }
}

impl From<usize> for Value {
    fn from(value: usize) -> Self {
        Value::Int(value as i64)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Value::Double(value)
    }
}

/// An ordered attribute list. Order is kept as written so a span reads the same
/// way in a collector's log every time.
pub type Attributes = Vec<(String, Value)>;

/// Append a `repeated KeyValue` under `field`.
pub fn encode_attributes(encoder: &mut Encoder, field: u32, attributes: &[(String, Value)]) {
    for (key, value) in attributes {
        let mut pair = Encoder::new();
        pair.string(1, key);
        pair.message(2, &value.encode());
        encoder.message(field, &pair);
    }
}

/// The same list in the JSON mapping.
pub fn attributes_json(attributes: &[(String, Value)]) -> Json {
    Json::Array(
        attributes
            .iter()
            .map(|(key, value)| {
                Json::object([("key", Json::from(key.clone())), ("value", value.to_json())])
            })
            .collect(),
    )
}

/// Who is reporting: the service, its environment, and the SDK behind it.
///
/// A collector groups everything it receives by resource, so getting
/// `service.name` right is what decides whether traces land under the right
/// service or under `unknown_service`.
#[derive(Debug, Clone)]
pub struct Resource {
    pub attributes: Attributes,
}

impl Resource {
    pub fn new(service: &str) -> Self {
        Resource {
            attributes: vec![
                ("service.name".to_string(), Value::from(service)),
                ("telemetry.sdk.name".to_string(), Value::from(SCOPE_NAME)),
                ("telemetry.sdk.language".to_string(), Value::from("rust")),
                ("telemetry.sdk.version".to_string(), Value::from(SCOPE_VERSION)),
            ],
        }
    }

    /// Add an attribute, replacing any earlier one with the same key so a
    /// caller's `service.name` wins over the default rather than sitting beside
    /// it — duplicate resource keys are undefined behaviour in OTLP.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        let key = key.into();
        let value = value.into();
        match self.attributes.iter_mut().find(|(existing, _)| *existing == key) {
            Some(slot) => slot.1 = value,
            None => self.attributes.push((key, value)),
        }
        self
    }

    /// Parse `OTEL_RESOURCE_ATTRIBUTES`: comma-separated `key=value` pairs.
    ///
    /// Values are percent-decoded, as the specification requires, so an
    /// attribute may contain a comma or an equals sign.
    pub fn with_pairs(mut self, pairs: &str) -> Self {
        for pair in pairs.split(',') {
            let pair = pair.trim();
            if let Some((key, value)) = pair.split_once('=')
                && !key.trim().is_empty()
            {
                self = self.with(key.trim().to_string(), percent_decode(value.trim()));
            }
        }
        self
    }

    pub fn service_name(&self) -> &str {
        self.attributes
            .iter()
            .find(|(key, _)| key == "service.name")
            .and_then(|(_, value)| match value {
                Value::String(text) => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("unknown_service")
    }

    pub fn encode(&self) -> Encoder {
        let mut resource = Encoder::new();
        encode_attributes(&mut resource, 1, &self.attributes);
        resource
    }

    pub fn to_json(&self) -> Json {
        Json::object([("attributes", attributes_json(&self.attributes))])
    }
}

/// Percent-decoding, for the two environment variables the specification
/// defines as percent-encoded (`OTEL_RESOURCE_ATTRIBUTES` and
/// `OTEL_EXPORTER_OTLP_HEADERS`).
///
/// Invalid escapes are left as written rather than dropped: a header value
/// containing a stray `%` is far more likely than a caller who meant to encode
/// something and got it wrong, and silently deleting a character from a
/// credential produces a 401 nobody can explain.
pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// The `InstrumentationScope` every span and metric here is attributed to.
pub fn scope() -> Encoder {
    let mut scope = Encoder::new();
    scope.string(1, SCOPE_NAME);
    scope.string(2, SCOPE_VERSION);
    scope
}

pub fn scope_json() -> Json {
    Json::object([
        ("name", Json::from(SCOPE_NAME)),
        ("version", Json::from(SCOPE_VERSION)),
    ])
}

/// Unix nanoseconds, which is the only clock OTLP speaks.
///
/// A time before the epoch saturates to zero rather than wrapping: OTLP reads
/// the field as unsigned, so a wrapped value would arrive as a timestamp in the
/// year 2554 and drag a whole trace out of every sensible query window.
pub fn unix_nanos(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map(|since| since.as_nanos() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_attribute_encodes_as_key_then_any_value() {
        let mut encoder = Encoder::new();
        encode_attributes(&mut encoder, 1, &[("k".to_string(), Value::from("v"))]);

        // KeyValue field 1, eight bytes: key "k" (field 1) then AnyValue
        // (field 2) holding string_value (field 1) "v".
        assert_eq!(encoder.as_bytes(), [0x0a, 0x08, 0x0a, 0x01, b'k', 0x12, 0x03, 0x0a, 0x01, b'v']);
    }

    #[test]
    fn an_integer_attribute_uses_int_value_not_double_value() {
        let mut encoder = Encoder::new();
        encode_attributes(&mut encoder, 1, &[("status".to_string(), Value::Int(200))]);

        // The AnyValue body must be field 3 (int_value), tag 0x18, varint 200.
        let bytes = encoder.into_bytes();
        assert!(bytes.ends_with(&[0x18, 0xc8, 0x01]), "{bytes:02x?}");
    }

    /// `AnyValue` is a `oneof`. A collector shows an attribute whose arm was
    /// never tagged as `Empty()`, and answers 200 while doing it, so a false
    /// boolean and an empty string have to reach the wire.
    #[test]
    fn a_default_valued_attribute_is_still_written() {
        for (value, expected) in [
            (Value::Bool(false), vec![0x10, 0x00]),
            (Value::Int(0), vec![0x18, 0x00]),
            (Value::String(String::new()), vec![0x0a, 0x00]),
        ] {
            let mut encoder = Encoder::new();
            encode_attributes(&mut encoder, 1, &[("k".to_string(), value.clone())]);
            let bytes = encoder.into_bytes();

            assert!(bytes.ends_with(&expected), "{value:?} vanished: {bytes:02x?}");
        }
    }

    #[test]
    fn json_numbers_narrow_to_integers_when_they_are_whole() {
        assert_eq!(Value::from_json(&Json::Number(3.0)), Value::Int(3));
        assert_eq!(Value::from_json(&Json::Number(3.5)), Value::Double(3.5));
        assert_eq!(Value::from_json(&Json::from("x")), Value::String("x".into()));
        assert_eq!(Value::from_json(&Json::from(true)), Value::Bool(true));
    }

    #[test]
    fn the_json_mapping_renders_integers_as_strings() {
        let rendered = attributes_json(&[("n".to_string(), Value::Int(7))]).to_string();

        assert!(rendered.contains(r#""intValue":"7""#), "{rendered}");
    }

    #[test]
    fn the_json_mapping_keeps_doubles_as_numbers() {
        let rendered = attributes_json(&[("d".to_string(), Value::Double(1.5))]).to_string();

        assert!(rendered.contains(r#""doubleValue":1.5"#), "{rendered}");
    }

    #[test]
    fn a_resource_attribute_replaces_rather_than_duplicates() {
        let resource = Resource::new("first").with("service.name", "second");

        assert_eq!(resource.service_name(), "second");
        assert_eq!(resource.attributes.iter().filter(|(k, _)| k == "service.name").count(), 1);
    }

    #[test]
    fn resource_attribute_pairs_are_parsed_and_percent_decoded() {
        let resource = Resource::new("api").with_pairs("deployment.environment=prod,team=a%20b");

        assert_eq!(
            resource.attributes.iter().find(|(k, _)| k == "team").map(|(_, v)| v.clone()),
            Some(Value::from("a b"))
        );
        assert_eq!(
            resource
                .attributes
                .iter()
                .find(|(k, _)| k == "deployment.environment")
                .map(|(_, v)| v.clone()),
            Some(Value::from("prod"))
        );
    }

    #[test]
    fn a_malformed_percent_escape_is_left_alone_rather_than_swallowed() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("a%2Cb"), "a,b");
    }

    #[test]
    fn attribute_values_have_a_total_order_even_with_nan() {
        let mut values = [Value::Double(f64::NAN), Value::Int(1), Value::from("a")];
        values.sort();

        // The point is only that sorting terminated and stayed consistent.
        assert_eq!(values.len(), 3);
        assert_eq!(values[0], Value::from("a"));
    }

    #[test]
    fn a_time_before_the_epoch_saturates_instead_of_wrapping() {
        let before = UNIX_EPOCH - std::time::Duration::from_secs(1);

        assert_eq!(unix_nanos(before), 0);
    }
}
