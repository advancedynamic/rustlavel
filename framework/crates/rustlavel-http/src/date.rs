//! HTTP dates: `Date`, `Last-Modified`, `If-Modified-Since`, `Expires`.
//!
//! RFC 9110 §5.6.7 has one format for sending — IMF-fixdate,
//! `Sun, 06 Nov 1994 08:49:37 GMT` — and two obsolete ones a recipient must
//! still accept: RFC 850's `Sunday, 06-Nov-94 08:49:37 GMT` and C's `asctime`,
//! `Sun Nov  6 08:49:37 1994`. Nothing has sent the old two in decades, but a
//! validator that rejects them makes a conditional request unconditional, and
//! the client pays for a body it already had.

/// Format a unix timestamp as an HTTP date (RFC 7231 IMF-fixdate).
pub fn http_date(unix: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let days_since_epoch = unix.div_euclid(86_400);
    let seconds_of_day = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days_since_epoch);

    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[(days_since_epoch.rem_euclid(7)) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60,
    )
}

/// Days since the unix epoch to a civil (year, month, day).
///
/// Howard Hinnant's `civil_from_days`, shifted to a March-based year so leap
/// days land at the end and need no special case.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}


/// Parse any of the three HTTP date formats to a unix timestamp.
///
/// Returns `None` for anything malformed — a conditional header that cannot be
/// read is treated as absent, which is what RFC 9110 §13.1.3 asks for.
pub fn parse_http_date(text: &str) -> Option<i64> {
    let text = text.trim();
    let parts: Vec<&str> = text.split_whitespace().collect();

    let (day, month, year, clock) = match parts.as_slice() {
        // IMF-fixdate: Sun, 06 Nov 1994 08:49:37 GMT
        [weekday, day, month, year, clock, "GMT"] if weekday.ends_with(',') => {
            (day.parse::<u32>().ok()?, month_number(month)?, year.parse::<i64>().ok()?, *clock)
        }
        // RFC 850: Sunday, 06-Nov-94 08:49:37 GMT
        [weekday, date, clock, "GMT"] if weekday.ends_with(',') => {
            let mut pieces = date.split('-');
            let day = pieces.next()?.parse::<u32>().ok()?;
            let month = month_number(pieces.next()?)?;
            let year = pieces.next()?.parse::<i64>().ok()?;
            if pieces.next().is_some() {
                return None;
            }
            // Two-digit years: RFC 9110 §5.6.7 says to read them as the most
            // recent year in the past with that ending, and this is close enough
            // for a format that was obsolete before most of today's web existed.
            let year = if year < 100 { if year < 70 { 2000 + year } else { 1900 + year } } else { year };
            (day, month, year, *clock)
        }
        // asctime: Sun Nov  6 08:49:37 1994
        [_weekday, month, day, clock, year] => {
            (day.parse::<u32>().ok()?, month_number(month)?, year.parse::<i64>().ok()?, *clock)
        }
        _ => return None,
    };

    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    if day == 0 || day > 31 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = name.to_ascii_lowercase();
    MONTHS.iter().position(|m| *m == lower).map(|i| i as u32 + 1)
}

/// The inverse of [`civil_from_days`], from the same source.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_http_dates() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_000_000_000), "Sun, 09 Sep 2001 01:46:40 GMT");
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
    }

    #[test]
    fn parses_all_three_formats_to_the_same_instant() {
        // The three examples RFC 9110 §5.6.7 gives, which all name one moment.
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), Some(784_111_777));
    }

    #[test]
    fn formatting_and_parsing_round_trip() {
        for unix in [0, 1, 86_399, 951_782_400, 1_000_000_000, 1_709_164_800, 4_102_444_800] {
            assert_eq!(parse_http_date(&http_date(unix)), Some(unix), "{unix}");
        }
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        assert_eq!(parse_http_date(""), None);
        assert_eq!(parse_http_date("yesterday"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 PST"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 25:00:00 GMT"), None);
        assert_eq!(parse_http_date("Sun, 32 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(parse_http_date("Sun, 06 Foo 1994 08:49:37 GMT"), None);
    }
}
