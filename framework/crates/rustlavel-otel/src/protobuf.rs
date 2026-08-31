//! Just enough protobuf to write OTLP.
//!
//! Rule one applies: protobuf is a serialisation format against a schema this
//! crate already knows at compile time, so it is written here rather than
//! pulled in with a code generator and a runtime library. Only the *encoding*
//! half exists — nothing in OTLP export ever has to read a protobuf message,
//! because the collector's reply is a status code and, at worst, a partial
//! success body nobody needs to decode field by field.
//!
//! The failure this module is guarding against is silent. A collector that
//! cannot parse a payload answers 400 and drops it; a collector that parses a
//! *wrong* field number accepts the payload and quietly loses whatever was in
//! it. Byte-level tests are the only way to know which one happened, so the
//! tests below check exact sequences taken from the protobuf encoding
//! specification rather than round-tripping through this same code.

/// Wire types, as protobuf numbers them. The tag byte of a field is its number
/// shifted left three, with the wire type in the low bits.
const VARINT: u32 = 0;
const FIXED64: u32 = 1;
const LENGTH_DELIMITED: u32 = 2;

/// A protobuf message under construction.
///
/// Sub-messages are built as their own `Encoder` and then handed to
/// [`Encoder::message`], because a length-delimited field has to know its
/// length before it writes its body and the only honest way to know that is to
/// have encoded the body already.
#[derive(Debug, Default, Clone)]
pub struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Encoder::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// A field key: the field number and the wire type, as one varint.
    fn tag(&mut self, field: u32, wire: u32) {
        self.raw_varint(u64::from(field) << 3 | u64::from(wire));
    }

    /// Base 128, little-endian, high bit set on every byte but the last.
    fn raw_varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.bytes.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.bytes.push(value as u8);
    }

    /// A `uint32`/`uint64` field. Zero is proto3's default for these, and a
    /// field left out decodes back to the default, so writing it would only
    /// make the payload bigger.
    pub fn uint64(&mut self, field: u32, value: u64) {
        if value != 0 {
            self.tag(field, VARINT);
            self.raw_varint(value);
        }
    }

    pub fn uint32(&mut self, field: u32, value: u32) {
        self.uint64(field, u64::from(value));
    }

    /// An `int64` field. Negative values sign-extend to a full 64 bits, which
    /// is why protobuf spends ten bytes on `-1` — `sint64` is the zig-zag
    /// alternative, and OTLP does not use it anywhere this crate encodes.
    pub fn int64(&mut self, field: u32, value: i64) {
        if value != 0 {
            self.tag(field, VARINT);
            self.raw_varint(value as u64);
        }
    }

    pub fn bool(&mut self, field: u32, value: bool) {
        if value {
            self.bool_present(field, value);
        }
    }

    /// Fields inside a `oneof` have explicit presence, so their default value
    /// still has to be written.
    ///
    /// This is not a micro-optimisation in reverse — it is the difference
    /// between an attribute arriving as `false` and arriving as nothing at all.
    /// A decoder tracks which arm of a `oneof` is set by which tag it saw, so
    /// omitting `bool_value = false` sets no arm and the collector prints
    /// `Empty()`. OTLP's `AnyValue` and `NumberDataPoint.value` are both
    /// `oneof`s, and a real collector is where this was caught.
    pub fn bool_present(&mut self, field: u32, value: bool) {
        self.tag(field, VARINT);
        self.raw_varint(u64::from(value));
    }

    /// See [`Encoder::bool_present`] — `AnyValue.int_value`.
    pub fn int64_present(&mut self, field: u32, value: i64) {
        self.tag(field, VARINT);
        self.raw_varint(value as u64);
    }

    /// See [`Encoder::bool_present`] — `AnyValue.string_value`.
    pub fn string_present(&mut self, field: u32, value: &str) {
        self.tag(field, LENGTH_DELIMITED);
        self.raw_varint(value.len() as u64);
        self.bytes.extend_from_slice(value.as_bytes());
    }

    /// See [`Encoder::bool_present`] — `NumberDataPoint.as_int`.
    pub fn sfixed64_present(&mut self, field: u32, value: i64) {
        self.tag(field, FIXED64);
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// An enum field. Enums travel as varints, and every OTLP enum reserves
    /// zero for "unspecified", so omitting zero is right here too.
    pub fn enumeration(&mut self, field: u32, value: i32) {
        if value != 0 {
            self.tag(field, VARINT);
            self.raw_varint(value as u32 as u64);
        }
    }

    /// A `fixed64` field: eight little-endian bytes, no varint compression.
    /// OTLP uses it for timestamps, where the values are large enough that a
    /// varint would cost more than eight bytes anyway.
    pub fn fixed64(&mut self, field: u32, value: u64) {
        if value != 0 {
            self.tag(field, FIXED64);
            self.bytes.extend_from_slice(&value.to_le_bytes());
        }
    }

    /// A `sfixed64` field — `NumberDataPoint.as_int`, and nothing else here.
    pub fn sfixed64(&mut self, field: u32, value: i64) {
        if value != 0 {
            self.tag(field, FIXED64);
            self.bytes.extend_from_slice(&value.to_le_bytes());
        }
    }

    /// A `double` field, IEEE 754 little-endian.
    pub fn double(&mut self, field: u32, value: f64) {
        if value != 0.0 {
            self.present_double(field, value);
        }
    }

    /// A `double` field that is written even when it holds the default.
    ///
    /// Proto3 `optional double` has explicit presence, and OTLP uses it for
    /// `HistogramDataPoint.sum`, `min` and `max`. There, absent means "this
    /// histogram does not report a sum" while zero means "the sum is zero" —
    /// two different facts that the default-skipping [`Encoder::double`] would
    /// collapse into one.
    pub fn present_double(&mut self, field: u32, value: f64) {
        self.tag(field, FIXED64);
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// A `string` field: length-delimited UTF-8.
    pub fn string(&mut self, field: u32, value: &str) {
        if !value.is_empty() {
            self.bytes_field(field, value.as_bytes());
        }
    }

    /// A `bytes` field. Named to leave `bytes` free for the buffer itself.
    pub fn bytes_field(&mut self, field: u32, value: &[u8]) {
        if value.is_empty() {
            return;
        }
        self.tag(field, LENGTH_DELIMITED);
        self.raw_varint(value.len() as u64);
        self.bytes.extend_from_slice(value);
    }

    /// A nested message.
    ///
    /// Written even when the sub-message encoded to nothing: message fields
    /// have explicit presence in proto3, so an empty `Status` still says "this
    /// span carries a status" rather than "no status was set". Callers that
    /// mean the second one simply do not call this.
    pub fn message(&mut self, field: u32, message: &Encoder) {
        self.tag(field, LENGTH_DELIMITED);
        self.raw_varint(message.bytes.len() as u64);
        self.bytes.extend_from_slice(&message.bytes);
    }

    /// A packed `repeated fixed64` — `HistogramDataPoint.bucket_counts`.
    ///
    /// Packed means one length-delimited field holding every element back to
    /// back, rather than one tag per element. Proto3 packs repeated scalars by
    /// default, so a collector expects exactly this.
    pub fn packed_fixed64(&mut self, field: u32, values: &[u64]) {
        if values.is_empty() {
            return;
        }
        self.tag(field, LENGTH_DELIMITED);
        self.raw_varint((values.len() * 8) as u64);
        for value in values {
            self.bytes.extend_from_slice(&value.to_le_bytes());
        }
    }

    /// A packed `repeated double` — `HistogramDataPoint.explicit_bounds`.
    pub fn packed_double(&mut self, field: u32, values: &[f64]) {
        if values.is_empty() {
            return;
        }
        self.tag(field, LENGTH_DELIMITED);
        self.raw_varint((values.len() * 8) as u64);
        for value in values {
            self.bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint_of(value: u64) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.raw_varint(value);
        encoder.into_bytes()
    }

    /// The worked example in the protobuf encoding specification: 300 is
    /// `1010 1100 0000 0010`, seven bits at a time, low group first, with the
    /// continuation bit set on every byte except the last.
    #[test]
    fn varints_match_the_specifications_worked_example() {
        assert_eq!(varint_of(0), [0x00]);
        assert_eq!(varint_of(1), [0x01]);
        assert_eq!(varint_of(127), [0x7f]);
        assert_eq!(varint_of(128), [0x80, 0x01]);
        assert_eq!(varint_of(300), [0xac, 0x02]);
        assert_eq!(varint_of(u64::MAX), [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
    }

    /// Also from the specification: field 1, wire type 0, value 150 is the
    /// three bytes `08 96 01`.
    #[test]
    fn a_tagged_varint_field_is_the_canonical_three_bytes() {
        let mut encoder = Encoder::new();
        encoder.uint64(1, 150);

        assert_eq!(encoder.as_bytes(), [0x08, 0x96, 0x01]);
    }

    /// The tag packs the field number above the three wire-type bits, so field
    /// 2 with a length-delimited body is `(2 << 3) | 2` = 0x12, then the length.
    #[test]
    fn a_string_field_is_tag_length_then_utf8() {
        let mut encoder = Encoder::new();
        encoder.string(2, "testing");

        assert_eq!(encoder.as_bytes(), b"\x12\x07testing");
    }

    #[test]
    fn a_nested_message_is_length_delimited_around_its_own_bytes() {
        let mut inner = Encoder::new();
        inner.uint64(1, 150);

        let mut outer = Encoder::new();
        outer.message(3, &inner);

        // Field 3, wire type 2, three bytes of body, then the body itself.
        assert_eq!(outer.as_bytes(), [0x1a, 0x03, 0x08, 0x96, 0x01]);
    }

    /// An empty sub-message still writes its tag and a zero length, because
    /// "present but default" and "absent" are different to a decoder.
    #[test]
    fn an_empty_nested_message_still_writes_its_header() {
        let mut outer = Encoder::new();
        outer.message(15, &Encoder::new());

        assert_eq!(outer.as_bytes(), [0x7a, 0x00]);
    }

    #[test]
    fn fixed64_is_eight_little_endian_bytes() {
        let mut encoder = Encoder::new();
        // Field 7 (`Span.start_time_unix_nano`), wire type 1: (7 << 3) | 1.
        encoder.fixed64(7, 1);

        assert_eq!(encoder.as_bytes(), [0x39, 0x01, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn a_double_is_its_ieee_754_bits_little_endian() {
        let mut encoder = Encoder::new();
        // Field 4 (`NumberDataPoint.as_double`), wire type 1: (4 << 3) | 1.
        encoder.double(4, 1.0);

        assert_eq!(encoder.as_bytes(), [0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f]);
    }

    /// The distinction the histogram sum depends on: a plain double vanishes at
    /// zero, an explicitly present one does not.
    #[test]
    fn zero_is_skipped_by_double_and_kept_by_present_double() {
        let mut skipped = Encoder::new();
        skipped.double(5, 0.0);
        assert!(skipped.is_empty());

        let mut kept = Encoder::new();
        kept.present_double(5, 0.0);
        assert_eq!(kept.as_bytes(), [0x29, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// The same distinction for the other three, which is what `oneof` members
    /// need. A real collector prints `Empty()` for an attribute whose arm was
    /// never tagged, and that is silent data loss with a 200 in front of it.
    #[test]
    fn oneof_members_are_written_even_when_they_hold_the_default() {
        let mut boolean = Encoder::new();
        boolean.bool(2, false);
        assert!(boolean.is_empty());
        boolean.bool_present(2, false);
        assert_eq!(boolean.as_bytes(), [0x10, 0x00]);

        let mut integer = Encoder::new();
        integer.int64_present(3, 0);
        assert_eq!(integer.as_bytes(), [0x18, 0x00]);

        let mut text = Encoder::new();
        text.string_present(1, "");
        assert_eq!(text.as_bytes(), [0x0a, 0x00]);

        let mut number = Encoder::new();
        number.sfixed64_present(6, 0);
        assert_eq!(number.as_bytes(), [0x31, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// Negative `int64` sign-extends rather than zig-zagging, so -1 is nine
    /// 0xff bytes and a 0x01. Getting this wrong would turn a small negative
    /// number into an enormous positive one.
    #[test]
    fn a_negative_int64_sign_extends_to_ten_bytes() {
        let mut encoder = Encoder::new();
        encoder.int64(1, -1);

        assert_eq!(encoder.as_bytes(), [0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
    }

    #[test]
    fn packed_repeated_scalars_share_one_tag_and_one_length() {
        let mut encoder = Encoder::new();
        // Field 6 (`bucket_counts`), wire type 2, two elements of eight bytes.
        encoder.packed_fixed64(6, &[1, 2]);

        let mut expected = vec![0x32, 0x10];
        expected.extend_from_slice(&1u64.to_le_bytes());
        expected.extend_from_slice(&2u64.to_le_bytes());
        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }

    #[test]
    fn packed_doubles_are_bounds_as_the_collector_expects_them() {
        let mut encoder = Encoder::new();
        encoder.packed_double(7, &[0.005, 0.01]);

        let mut expected = vec![0x3a, 0x10];
        expected.extend_from_slice(&0.005f64.to_le_bytes());
        expected.extend_from_slice(&0.01f64.to_le_bytes());
        assert_eq!(encoder.as_bytes(), expected.as_slice());
    }

    /// Proto3 leaves defaults out. An all-default message is zero bytes, and a
    /// decoder rebuilds the defaults on the other side.
    #[test]
    fn default_values_are_omitted_entirely() {
        let mut encoder = Encoder::new();
        encoder.uint64(1, 0);
        encoder.int64(2, 0);
        encoder.bool(3, false);
        encoder.enumeration(4, 0);
        encoder.fixed64(5, 0);
        encoder.string(6, "");
        encoder.bytes_field(7, &[]);
        encoder.packed_fixed64(8, &[]);

        assert!(encoder.is_empty());
    }

    /// Field numbers above 15 need a two-byte tag, and OTLP has several — the
    /// span status is field 15, and `Span.flags` is 16.
    #[test]
    fn field_numbers_past_fifteen_use_a_two_byte_tag() {
        let mut encoder = Encoder::new();
        encoder.uint64(16, 1);

        // (16 << 3) | 0 = 128, which needs a continuation byte of its own.
        assert_eq!(encoder.as_bytes(), [0x80, 0x01, 0x01]);
    }
}
