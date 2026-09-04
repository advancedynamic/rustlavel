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
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
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

/// How Settings → General says to write a date and a time.
///
/// Three settings that were on the tab and read by nothing: `app.date_format`,
/// `app.time_format` and `app.timezone`. A date on an administration page goes
/// through here now, so choosing `DD/MM/YYYY` on that tab changes what the
/// pages show rather than storing a preference nobody consults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dates {
    pub date: String,
    pub time: String,
    /// The zone rows are read in. Stored rows are UTC; this is what shifts
    /// them, and it is the zone rather than an offset because two of the
    /// zones on the tab observe summer time.
    pub zone: String,
}

impl Default for Dates {
    fn default() -> Dates {
        Dates { date: "d M Y".into(), time: "24".into(), zone: "UTC".into() }
    }
}

/// Minutes to add to UTC to read a moment in `zone`.
///
/// **Takes the instant, because two of the zones on the tab move.** The first
/// version of this was `offset_of(zone)` with a fixed number per zone, and it
/// knew Kuala Lumpur and Bangkok — which the tab does not offer — while
/// Europe/London and America/New_York, which it does, fell through to zero. A
/// person in New York saw UTC and a person in London saw the right time for
/// four months of the year.
///
/// There is no time-zone database here, and there does not need to be for
/// seven zones: the Asian ones have had one offset since 1988, and the two
/// that observe summer time follow rules written in law rather than in a file.
pub fn offset_at(zone: &str, unix: i64) -> i64 {
    match zone {
        "Asia/Jakarta" | "Asia/Bangkok" => 7 * 60,
        "Asia/Makassar" | "Asia/Singapore" | "Asia/Kuala_Lumpur" => 8 * 60,
        "Asia/Jayapura" => 9 * 60,
        // British Summer Time: 01:00 UTC on the last Sunday in March until
        // 01:00 UTC on the last Sunday in October. The whole EU switches at
        // the same instant, which is why the rule is written in UTC.
        "Europe/London" => {
            if unix >= sunday(year_of(unix), 3, Week::Last) * 86_400 + 3_600
                && unix < sunday(year_of(unix), 10, Week::Last) * 86_400 + 3_600
            {
                60
            } else {
                0
            }
        }
        // US Eastern: 02:00 local on the second Sunday in March (07:00 UTC,
        // still on standard time) until 02:00 local on the first Sunday in
        // November (06:00 UTC, still on daylight time).
        "America/New_York" => {
            if unix >= sunday(year_of(unix), 3, Week::Second) * 86_400 + 7 * 3_600
                && unix < sunday(year_of(unix), 11, Week::First) * 86_400 + 6 * 3_600
            {
                -4 * 60
            } else {
                -5 * 60
            }
        }
        _ => 0,
    }
}

enum Week {
    First,
    Second,
    Last,
}

fn year_of(unix: i64) -> i64 {
    crate::support::tokens::civil_from_days(unix.div_euclid(86_400)).0
}

/// The days-since-epoch of a Sunday in `month`.
///
/// 1970-01-01 was a Thursday, so `(days + 4) % 7` is 0 on a Sunday.
fn sunday(year: i64, month: u32, which: Week) -> i64 {
    match which {
        Week::First | Week::Second => {
            let first = crate::support::tokens::days_from_civil(year, month, 1);
            let offset = (7 - (first + 4).rem_euclid(7)).rem_euclid(7);
            first + offset + if matches!(which, Week::Second) { 7 } else { 0 }
        }
        Week::Last => {
            let days_in = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][(month - 1) as usize];
            let leap = month == 2 && (year % 4 == 0 && (year % 100 != 0 || year % 400 == 0));
            let last = crate::support::tokens::days_from_civil(year, month, days_in + u32::from(leap));
            last - (last + 4).rem_euclid(7)
        }
    }
}

impl Dates {
    pub async fn of(req: &Request) -> Dates {
        match req.state::<Settings>() {
            Some(settings) => Dates {
                date: settings.get("app.date_format").await,
                time: settings.get("app.time_format").await,
                zone: settings.get("app.timezone").await,
            },
            None => Dates::default(),
        }
    }

    /// `2026-09-03 04:13:27` as the settings ask for it.
    pub fn moment(&self, stored: &str) -> String {
        let unix = crate::support::tokens::parse_utc(stored);
        if unix == 0 {
            return "—".to_string();
        }
        let shifted = unix + offset_at(&self.zone, unix) * 60;
        format!("{} at {}", self.day(shifted), self.clock(shifted))
    }

    /// How long ago, in words, for the line under a timestamp.
    ///
    /// `now` is a parameter rather than a call to the clock so this can be
    /// tested at all — the same reason `schedule::next_due` takes one. Past a
    /// week the words stop helping and the date is what a person wants, so
    /// that is what they get, written the way the tab asks for.
    pub fn ago(&self, stored: &str, now: &str) -> String {
        let unix = crate::support::tokens::parse_utc(stored);
        let now = crate::support::tokens::parse_utc(now);
        if unix == 0 || now == 0 {
            return "—".to_string();
        }

        // A row stamped a second into the future — two clocks disagreeing by a
        // little — reads as "just now" rather than as a negative age.
        let seconds = (now - unix).max(0);
        let plural = |n: i64, unit: &str| {
            format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
        };
        match seconds {
            0..=59 => "just now".to_string(),
            60..=3_599 => plural(seconds / 60, "minute"),
            3_600..=86_399 => plural(seconds / 3_600, "hour"),
            86_400..=604_799 => plural(seconds / 86_400, "day"),
            _ => self.day_of(stored),
        }
    }

    /// The date alone, for a column where the time adds nothing.
    pub fn day_of(&self, stored: &str) -> String {
        let unix = crate::support::tokens::parse_utc(stored);
        if unix == 0 {
            return "—".to_string();
        }
        self.day(unix + offset_at(&self.zone, unix) * 60)
    }

    fn day(&self, unix: i64) -> String {
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let (year, month, day) = crate::support::tokens::civil_from_days(unix.div_euclid(86_400));
        // The values the catalogue stores, which are PHP's date letters —
        // not the `DD/MM/YYYY` labels the dropdown shows a person. Matching
        // the labels is what made three of the four formats do nothing.
        match self.date.as_str() {
            "d/m/Y" => format!("{day:02}/{month:02}/{year}"),
            "m/d/Y" => format!("{month:02}/{day:02}/{year}"),
            "Y-m-d" => format!("{year}-{month:02}-{day:02}"),
            _ => format!("{day} {} {year}", MONTHS[(month - 1) as usize]),
        }
    }

    fn clock(&self, unix: i64) -> String {
        let seconds = unix.rem_euclid(86_400);
        let (hour, minute) = (seconds / 3600, (seconds % 3600) / 60);
        match self.time.as_str() {
            "12" => {
                let shown = if hour % 12 == 0 { 12 } else { hour % 12 };
                let half = if hour < 12 { "am" } else { "pm" };
                format!("{shown}:{minute:02} {half}")
            }
            _ => format!("{hour:02}:{minute:02}"),
        }
    }
}

#[cfg(test)]
mod date_tests {
    use super::*;

    #[test]
    fn the_date_format_setting_changes_what_is_shown() {
        let stored = "2026-09-03 04:13:27";
        let at = |d: &str| Dates { date: d.into(), ..Dates::default() }.day_of(stored);

        assert_eq!(at("d M Y"), "3 Sep 2026");
        assert_eq!(at("d/m/Y"), "03/09/2026");
        assert_eq!(at("m/d/Y"), "09/03/2026");
        assert_eq!(at("Y-m-d"), "2026-09-03");
    }

    /// The catalogue offers four; the formatter has to render four.
    ///
    /// The first version of this matched on the labels the dropdown shows
    /// rather than the values it saves, so `d/m/Y`, `m/d/Y` and `Y-m-d` all
    /// fell through to the default arm and the setting did nothing — and the
    /// test above, written against the same labels, agreed with the bug.
    /// Taking the inputs from the catalogue is what makes that impossible: the
    /// two lists cannot drift apart without a distinct rendering going
    /// missing.
    #[test]
    fn every_date_format_the_tab_offers_renders_differently() {
        let stored = "2026-09-03 04:13:27";
        let mut seen: Vec<(&str, String)> = Vec::new();

        for (value, label) in crate::support::settings::DATE_FORMATS {
            let shown = Dates { date: (*value).into(), ..Dates::default() }.day_of(stored);
            assert!(
                !seen.iter().any(|(_, other)| *other == shown),
                "`{value}` ({label}) renders as {shown:?}, the same as another format on the \
                 tab — the formatter does not know this value and fell through to its default"
            );
            seen.push((value, shown));
        }
        assert_eq!(seen.len(), crate::support::settings::DATE_FORMATS.len());
    }

    #[test]
    fn every_clock_the_tab_offers_renders_differently() {
        let mut seen: Vec<String> = Vec::new();
        for (value, label) in crate::support::settings::TIME_FORMATS {
            let shown = Dates { time: (*value).into(), ..Dates::default() }.moment("2026-09-03 16:05:00");
            assert!(!seen.contains(&shown), "`{value}` ({label}) renders as {shown:?}, like another");
            seen.push(shown);
        }
    }

    #[test]
    fn the_time_format_setting_changes_the_clock() {
        let noon = Dates { time: "12".into(), ..Dates::default() };
        let twenty_four = Dates::default();

        assert!(twenty_four.moment("2026-09-03 16:05:00").ends_with("16:05"));
        assert!(noon.moment("2026-09-03 16:05:00").ends_with("4:05 pm"));
        // Midnight and noon are the two a twelve-hour clock gets wrong.
        assert!(noon.moment("2026-09-03 00:30:00").ends_with("12:30 am"));
        assert!(noon.moment("2026-09-03 12:30:00").ends_with("12:30 pm"));
    }

    /// Rows are stored in UTC; the zone is what a person reads them in.
    #[test]
    fn the_timezone_shifts_the_moment_and_can_cross_a_day() {
        let jakarta = Dates { zone: "Asia/Jakarta".into(), ..Dates::default() };
        let winter = 1_767_225_600; // 2026-01-01 00:00 UTC

        assert_eq!(offset_at("Asia/Jakarta", winter), 420);
        assert_eq!(offset_at("UTC", winter), 0);
        assert_eq!(offset_at("nonsense", winter), 0);

        assert!(jakarta.moment("2026-09-03 04:13:27").starts_with("3 Sep 2026 at 11:13"));
        // 20:00 UTC is the next morning in Jakarta.
        assert_eq!(jakarta.day_of("2026-09-03 20:00:00"), "4 Sep 2026");
    }

    /// Every zone on the tab has to be a zone the formatter knows.
    ///
    /// The list and the `match` are in different files, and the first version
    /// of the match answered for two zones nobody could pick while answering
    /// UTC for two that anybody could. Only UTC is allowed to be zero.
    #[test]
    fn every_timezone_the_tab_offers_is_one_the_formatter_knows() {
        let january = 1_767_225_600; // 2026-01-01 00:00 UTC
        let july = 1_783_000_000; // 2026-07-05, in the northern summer

        for (zone, label) in crate::support::settings::TIMEZONES {
            // London really is UTC in January, so ask at both ends of the
            // year: a zone the formatter does not know answers zero at both.
            assert!(
                *zone == "UTC" || offset_at(zone, january) != 0 || offset_at(zone, july) != 0,
                "`{zone}` ({label}) is on the Timezone list and the formatter answers UTC for \
                 it all year, so choosing it does nothing"
            );
        }

        // The two that move, and the direction they move in.
        assert_eq!(offset_at("Europe/London", january), 0);
        assert_eq!(offset_at("Europe/London", july), 60);
        assert_eq!(offset_at("America/New_York", january), -300);
        assert_eq!(offset_at("America/New_York", july), -240);
        // Jakarta has never observed summer time.
        assert_eq!(offset_at("Asia/Jakarta", july), 420);
    }

    /// The exact instants the clocks go forward and back.
    #[test]
    fn summer_time_starts_and_ends_on_the_right_sunday() {
        // 2026: the last Sunday in March is the 29th, the last in October the
        // 25th; the second Sunday in March is the 8th and the first in
        // November the 1st.
        let at = |y: i64, m: u32, d: u32, h: i64| {
            crate::support::tokens::days_from_civil(y, m, d) * 86_400 + h * 3_600
        };

        assert_eq!(offset_at("Europe/London", at(2026, 3, 29, 0) + 3_599), 0);
        assert_eq!(offset_at("Europe/London", at(2026, 3, 29, 1)), 60);
        assert_eq!(offset_at("Europe/London", at(2026, 10, 25, 1) - 1), 60);
        assert_eq!(offset_at("Europe/London", at(2026, 10, 25, 1)), 0);

        assert_eq!(offset_at("America/New_York", at(2026, 3, 8, 7) - 1), -300);
        assert_eq!(offset_at("America/New_York", at(2026, 3, 8, 7)), -240);
        assert_eq!(offset_at("America/New_York", at(2026, 11, 1, 6) - 1), -240);
        assert_eq!(offset_at("America/New_York", at(2026, 11, 1, 6)), -300);
    }

    /// The second line under a timestamp says how long ago, not the same
    /// moment written out again — which is what it did when it called the
    /// absolute formatter.
    #[test]
    fn ago_counts_in_words_until_a_week_and_then_gives_the_date() {
        let dates = Dates::default();
        let ago = |stored: &str| dates.ago(stored, "2026-09-10 12:00:00");

        assert_eq!(ago("2026-09-10 11:59:30"), "just now");
        assert_eq!(ago("2026-09-10 11:59:00"), "1 minute ago");
        assert_eq!(ago("2026-09-10 11:30:00"), "30 minutes ago");
        assert_eq!(ago("2026-09-10 11:00:00"), "1 hour ago");
        assert_eq!(ago("2026-09-10 02:00:00"), "10 hours ago");
        assert_eq!(ago("2026-09-09 12:00:00"), "1 day ago");
        assert_eq!(ago("2026-09-04 12:00:00"), "6 days ago");
        // A week and beyond: the date, not "8 days ago".
        assert_eq!(ago("2026-09-03 12:00:00"), "3 Sep 2026");
        // Clocks that disagree by a second do not produce a negative age.
        assert_eq!(ago("2026-09-10 12:00:01"), "just now");
        assert_eq!(dates.ago("", "2026-09-10 12:00:00"), "—");
    }

    #[test]
    fn an_unparseable_timestamp_is_a_dash_rather_than_the_epoch() {
        assert_eq!(Dates::default().moment(""), "—");
        assert_eq!(Dates::default().day_of("not a date"), "—");
    }
}
