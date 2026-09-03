//! Writing a number the way Settings → Language says to.
//!
//! Here because a setting nothing reads is decoration, and this application
//! has spent enough of its life with switches that did nothing. Every count on
//! an administration page and every currency amount on the Backup tab goes
//! through this, so changing the format on that tab changes what the pages
//! show on the next request.

use rustlavel::prelude::*;

use crate::support::settings::Settings;

/// The separators for a named format.
///
/// `(thousands, decimal)`. `plain` has no grouping at all, which is what a
/// person exporting to a spreadsheet wants.
fn separators(format: &str) -> (&'static str, &'static str) {
    match format {
        "en" => (",", "."),
        "plain" => ("", "."),
        _ => (".", ","),
    }
}

/// A whole number, grouped.
pub fn integer(value: i64, format: &str) -> String {
    let (thousands, _) = separators(format);
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();

    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (index, digit) in digits.chars().enumerate() {
        // Grouped from the right, so the first separator falls where the
        // remaining digit count is a multiple of three.
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push_str(thousands);
        }
        out.push(digit);
    }
    match negative {
        true => format!("-{out}"),
        false => out,
    }
}

/// An amount with its currency prefix, to two decimals when it has them.
pub fn money(cents: i64, format: &str, currency: &str) -> String {
    let (_, decimal) = separators(format);
    let whole = integer(cents / 100, format);
    let fraction = (cents % 100).abs();

    match fraction {
        0 => format!("{currency}{whole}"),
        _ => format!("{currency}{whole}{decimal}{fraction:02}"),
    }
}

/// A byte count, which is the one number on these pages nobody wants grouped
/// into digits: 13 kB reads and 13.939 does not.
pub fn bytes(count: i64) -> String {
    const UNITS: [(&str, i64); 4] =
        [("GB", 1_073_741_824), ("MB", 1_048_576), ("kB", 1024), ("bytes", 1)];

    for (unit, size) in UNITS {
        if count >= size {
            return match size {
                1 => format!("{count} bytes"),
                _ => format!("{:.1} {unit}", count as f64 / size as f64),
            };
        }
    }
    "0 bytes".to_string()
}

/// The two settings this module needs, read once per request.
pub async fn preferences(req: &Request) -> (String, String) {
    match req.state::<Settings>() {
        Some(settings) => {
            (settings.get("app.number_format").await, settings.get("app.currency").await)
        }
        None => ("id".to_string(), "Rp ".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_is_grouped_the_way_the_setting_asks() {
        assert_eq!(integer(1_234_567, "id"), "1.234.567");
        assert_eq!(integer(1_234_567, "en"), "1,234,567");
        assert_eq!(integer(1_234_567, "plain"), "1234567");
    }

    /// The grouping is from the right, so every length has to land correctly —
    /// not just the ones that happen to be a multiple of three.
    #[test]
    fn every_length_groups_from_the_right() {
        for (value, expected) in [
            (0i64, "0"),
            (7, "7"),
            (99, "99"),
            (999, "999"),
            (1_000, "1.000"),
            (12_345, "12.345"),
            (100_000, "100.000"),
            (1_000_000, "1.000.000"),
        ] {
            assert_eq!(integer(value, "id"), expected, "{value}");
        }
    }

    #[test]
    fn a_negative_number_keeps_its_sign_outside_the_grouping() {
        assert_eq!(integer(-1_234_567, "id"), "-1.234.567");
        assert_eq!(integer(-999, "id"), "-999");
    }

    #[test]
    fn money_uses_the_decimal_separator_of_its_own_format() {
        assert_eq!(money(123_456_700, "id", "Rp "), "Rp 1.234.567");
        assert_eq!(money(123_456_789, "id", "Rp "), "Rp 1.234.567,89");
        assert_eq!(money(123_456_789, "en", "$"), "$1,234,567.89");
        assert_eq!(money(0, "id", "Rp "), "Rp 0");
    }

    #[test]
    fn a_byte_count_is_read_as_a_size_rather_than_grouped() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(13_939), "13.6 kB");
        assert_eq!(bytes(5_242_880), "5.0 MB");
        assert_eq!(bytes(2_147_483_648), "2.0 GB");
    }
}
