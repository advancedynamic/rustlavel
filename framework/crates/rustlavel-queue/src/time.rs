//! The small amount of calendar arithmetic a scheduler needs.
//!
//! The framework has no date type: rustlavel-db deliberately leaves a
//! `timestamptz` column as text so precision survives, and core has nothing
//! richer than `SystemTime`. A cron schedule cannot be computed without real
//! calendar maths, so this module carries the minimum — seconds since the Unix
//! epoch in and out of a civil UTC date, and nothing else.
//!
//! Everything the queue stores is epoch seconds rather than a timestamp column,
//! for the same reason: an integer compares and sorts identically in every
//! driver, and no part of the system has to agree on a text format.
//!
//! The conversions are Howard Hinnant's `days_from_civil` / `civil_from_days`,
//! chosen because they are exact for every year in range and short enough to
//! verify by reading.

/// Seconds in a day. Leap seconds do not exist in Unix time, so this is exact.
pub const SECONDS_PER_DAY: i64 = 86_400;

/// A civil date and time in UTC.
///
/// UTC and not local time on purpose: a schedule that silently shifts by an
/// hour twice a year, differently on every machine in a cluster, is a bug
/// generator. An application that wants local time can offset the expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i64,
    /// 1–12.
    pub month: u32,
    /// 1–31.
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = Sunday, matching cron's numbering rather than ISO's.
    pub weekday: u32,
}

/// Now, in seconds since the epoch.
///
/// A clock set before 1970 would make this negative, which every consumer here
/// handles, so there is no need to fail.
pub fn unix_now() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    }
}

/// Split epoch seconds into a civil UTC date and time.
pub fn from_unix(seconds: i64) -> DateTime {
    // `div_euclid` rather than `/` so times before 1970 round the right way.
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let time_of_day = seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);

    DateTime {
        year,
        month,
        day,
        hour: (time_of_day / 3600) as u32,
        minute: ((time_of_day % 3600) / 60) as u32,
        second: (time_of_day % 60) as u32,
        weekday: weekday_of(days),
    }
}

/// Build epoch seconds from a civil UTC date and time.
pub fn to_unix(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    days_from_civil(year, month, day) * SECONDS_PER_DAY
        + i64::from(hour) * 3600
        + i64::from(minute) * 60
        + i64::from(second)
}

/// The start of the day containing `seconds`.
pub fn start_of_day(seconds: i64) -> i64 {
    seconds - seconds.rem_euclid(SECONDS_PER_DAY)
}

/// The start of the minute containing `seconds`.
pub fn start_of_minute(seconds: i64) -> i64 {
    seconds - seconds.rem_euclid(60)
}

/// Day of the week for a day number, 0 = Sunday.
///
/// Day 0 of the epoch — 1 January 1970 — was a Thursday, which is where the
/// `+ 4` comes from.
fn weekday_of(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

/// Days since 1970-01-01 for a civil date.
///
/// The trick is shifting the year to start in March, which puts the leap day at
/// the end of the year and makes the month-length table a straight line.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let month = i64::from(month);
    let year = year - i64::from(month <= 2);

    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    era * 146_097 + day_of_era - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };

    (year + i64::from(month <= 2), month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_known_dates_in_both_directions() {
        // The epoch itself, a Thursday.
        let epoch = from_unix(0);
        assert_eq!((epoch.year, epoch.month, epoch.day), (1970, 1, 1));
        assert_eq!(epoch.weekday, 4);
        assert_eq!(to_unix(1970, 1, 1, 0, 0, 0), 0);

        // A date every developer can check against a wall calendar.
        let known = to_unix(2026, 8, 29, 13, 45, 30);
        let back = from_unix(known);
        assert_eq!((back.year, back.month, back.day), (2026, 8, 29));
        assert_eq!((back.hour, back.minute, back.second), (13, 45, 30));
        assert_eq!(back.weekday, 6, "29 August 2026 is a Saturday");
    }

    #[test]
    fn handles_leap_days_and_century_rules() {
        // 2000 was a leap year; 1900 was not.
        let leap = from_unix(to_unix(2000, 2, 29, 0, 0, 0));
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));

        let march_1900 = from_unix(to_unix(1900, 2, 28, 0, 0, 0) + SECONDS_PER_DAY);
        assert_eq!((march_1900.year, march_1900.month, march_1900.day), (1900, 3, 1));
    }

    #[test]
    fn round_trips_every_day_for_a_decade() {
        let mut seconds = to_unix(2020, 1, 1, 0, 0, 0);
        let end = to_unix(2030, 1, 1, 0, 0, 0);
        let mut previous_weekday = from_unix(seconds).weekday;

        while seconds < end {
            let date = from_unix(seconds);
            assert_eq!(
                to_unix(date.year, date.month, date.day, date.hour, date.minute, date.second),
                seconds
            );
            seconds += SECONDS_PER_DAY;

            let weekday = from_unix(seconds).weekday;
            assert_eq!(weekday, (previous_weekday + 1) % 7, "weekdays must advance by one");
            previous_weekday = weekday;
        }
    }

    #[test]
    fn dates_before_the_epoch_do_not_round_the_wrong_way() {
        let date = from_unix(-1);
        assert_eq!((date.year, date.month, date.day), (1969, 12, 31));
        assert_eq!((date.hour, date.minute, date.second), (23, 59, 59));
    }

    #[test]
    fn truncates_to_day_and_minute_boundaries() {
        let noon = to_unix(2026, 8, 29, 12, 34, 56);
        assert_eq!(start_of_day(noon), to_unix(2026, 8, 29, 0, 0, 0));
        assert_eq!(start_of_minute(noon), to_unix(2026, 8, 29, 12, 34, 0));
    }
}
