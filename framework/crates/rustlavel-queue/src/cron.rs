//! A cron expression parser and next-run calculator, written from scratch.
//!
//! Five fields, in the order every crontab has used since 1975:
//!
//! ```text
//! ┌───────────── minute        0-59
//! │ ┌─────────── hour          0-23
//! │ │ ┌───────── day of month  1-31
//! │ │ │ ┌─────── month         1-12
//! │ │ │ │ ┌───── day of week   0-7  (0 and 7 are both Sunday)
//! * * * * *
//! ```
//!
//! Each field accepts `*`, a number, a list (`1,15`), a range (`1-5`), a step
//! over the whole field (`*/5`) or over a range (`0-30/10`), and Vixie cron's
//! `5/10`, which means "from 5 to the end of the field, every 10".
//!
//! Anything else is an error rather than a silent default. A schedule that
//! quietly runs at the wrong time is worse than one that refuses to start.

use crate::time::{self, DateTime, SECONDS_PER_DAY};
use rustlavel_core::{Error, Result};

/// A day of the week, so `weekly_on` reads as English rather than as a number
/// the caller has to remember the base of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weekday {
    Sunday,
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
}

impl Weekday {
    /// Cron's numbering: 0 is Sunday.
    pub fn number(self) -> u32 {
        self as u32
    }
}

/// One field of an expression, as a bitmask over its allowed values.
///
/// A mask rather than a list because matching is then a single shift and test,
/// and because it collapses `1,1,1-3` to the set it actually describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Field {
    bits: u64,
}

impl Field {
    fn contains(self, value: u32) -> bool {
        value < 64 && self.bits & (1 << value) != 0
    }
}

/// A parsed cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: Field,
    hour: Field,
    day_of_month: Field,
    month: Field,
    day_of_week: Field,
    /// Whether the day fields were narrowed, which decides how they combine.
    dom_restricted: bool,
    dow_restricted: bool,
    expression: String,
}

impl Cron {
    /// Parse a five-field expression.
    pub fn parse(expression: &str) -> Result<Cron> {
        let fields: Vec<&str> = expression.split_whitespace().collect();

        if fields.len() != 5 {
            return Err(Error::msg(format!(
                "`{expression}` has {} field(s); a cron expression needs exactly 5: \
                 minute hour day-of-month month day-of-week",
                fields.len()
            )));
        }

        Ok(Cron {
            minute: parse_field(fields[0], 0, 59, "minute")?,
            hour: parse_field(fields[1], 0, 23, "hour")?,
            day_of_month: parse_field(fields[2], 1, 31, "day of month")?,
            month: parse_field(fields[3], 1, 12, "month")?,
            day_of_week: parse_weekday_field(fields[4])?,
            dom_restricted: fields[2] != "*",
            dow_restricted: fields[4] != "*",
            expression: fields.join(" "),
        })
    }

    /// The normalised expression this was parsed from.
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// Whether a given moment is one this expression selects.
    ///
    /// Seconds are ignored: cron's resolution is a minute.
    pub fn matches(&self, at: &DateTime) -> bool {
        self.date_matches(at) && self.hour.contains(at.hour) && self.minute.contains(at.minute)
    }

    /// The first matching moment strictly after `after`, in epoch seconds.
    ///
    /// `None` when the expression describes a date that never occurs — `0 0 30
    /// 2 *`, the thirtieth of February. Answering rather than looping forever
    /// is the whole reason this returns an `Option`.
    pub fn next_after(&self, after: i64) -> Option<i64> {
        let mut candidate = time::start_of_minute(after) + 60;

        // Four years covers a full leap cycle, so any date that can occur at
        // all occurs within the window; anything that does not is impossible.
        let limit = candidate + 4 * 366 * SECONDS_PER_DAY;

        while candidate < limit {
            let at = time::from_unix(candidate);

            if !self.date_matches(&at) {
                // Skip the rest of a day whose date can never match, instead of
                // testing its other 1439 minutes one at a time.
                candidate = time::start_of_day(candidate) + SECONDS_PER_DAY;
                continue;
            }
            if !self.hour.contains(at.hour) {
                candidate = time::start_of_day(candidate) + i64::from(at.hour + 1) * 3600;
                continue;
            }
            if self.minute.contains(at.minute) {
                return Some(candidate);
            }
            candidate += 60;
        }

        None
    }

    /// Month plus the day-of-month / day-of-week rule.
    fn date_matches(&self, at: &DateTime) -> bool {
        if !self.month.contains(at.month) {
            return false;
        }

        let by_day = self.day_of_month.contains(at.day);
        let by_weekday = self.day_of_week.contains(at.weekday);

        // Cron's oldest quirk: when *both* day fields are narrowed they are
        // combined with OR, not AND. `0 0 1 * 1` fires on the first of the
        // month *and* on every Monday. Faithfully reproduced here because an
        // expression copied from a crontab must mean what it meant there.
        match (self.dom_restricted, self.dow_restricted) {
            (true, true) => by_day || by_weekday,
            (true, false) => by_day,
            (false, true) => by_weekday,
            (false, false) => true,
        }
    }
}

/// Day of week, with 7 folded onto 0 so both spellings of Sunday work.
fn parse_weekday_field(spec: &str) -> Result<Field> {
    let field = parse_field(spec, 0, 7, "day of week")?;
    let bits = if field.contains(7) { (field.bits & !(1 << 7)) | 1 } else { field.bits };
    Ok(Field { bits })
}

/// Parse one comma-separated field into its bitmask.
fn parse_field(spec: &str, min: u32, max: u32, label: &str) -> Result<Field> {
    if spec.is_empty() {
        return Err(invalid(spec, label, min, max));
    }

    let mut bits = 0u64;

    for part in spec.split(',') {
        bits |= parse_part(part, min, max, label)?;
    }

    Ok(Field { bits })
}

/// One element of a list: `*`, `n`, `a-b`, or any of those with a `/step`.
fn parse_part(part: &str, min: u32, max: u32, label: &str) -> Result<u64> {
    let (range_spec, step) = match part.split_once('/') {
        Some((range_spec, step_spec)) => {
            let step: u32 = step_spec.parse().map_err(|_| invalid(part, label, min, max))?;
            if step == 0 {
                return Err(Error::msg(format!(
                    "`{part}` is not a valid {label} field: a step of 0 would select nothing"
                )));
            }
            (range_spec, step)
        }
        None => (part, 1),
    };

    // A bare `a/n` means "from a to the end of the field", which is why the
    // upper bound here is `max` rather than `a`.
    let (low, high) = if range_spec == "*" {
        (min, max)
    } else if let Some((start, end)) = range_spec.split_once('-') {
        let start: u32 = start.parse().map_err(|_| invalid(part, label, min, max))?;
        let end: u32 = end.parse().map_err(|_| invalid(part, label, min, max))?;
        if start > end {
            return Err(Error::msg(format!(
                "`{part}` is not a valid {label} field: the range starts after it ends"
            )));
        }
        (start, end)
    } else {
        let value: u32 = range_spec.parse().map_err(|_| invalid(part, label, min, max))?;
        if part.contains('/') { (value, max) } else { (value, value) }
    };

    if low < min || high > max {
        return Err(invalid(part, label, min, max));
    }

    let mut bits = 0u64;
    let mut value = low;
    while value <= high {
        bits |= 1 << value;
        value += step;
    }
    Ok(bits)
}

fn invalid(part: &str, label: &str, min: u32, max: u32) -> Error {
    Error::msg(format!(
        "`{part}` is not a valid {label} field. Use `*`, a number between {min} and {max}, \
         a list (`1,15`), a range (`1-5`), or a step (`*/5`)."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::to_unix;

    fn next(expression: &str, from: i64) -> i64 {
        Cron::parse(expression)
            .unwrap_or_else(|e| panic!("{expression} should parse: {e}"))
            .next_after(from)
            .unwrap_or_else(|| panic!("{expression} should have a next run"))
    }

    /// The minutes an expression selects, so a mask can be asserted directly.
    fn minutes_of(expression: &str) -> Vec<u32> {
        let cron = Cron::parse(expression).unwrap();
        (0..60).filter(|m| cron.minute.contains(*m)).collect()
    }

    #[test]
    fn a_star_field_selects_everything() {
        assert_eq!(minutes_of("* * * * *"), (0..60).collect::<Vec<_>>());
    }

    #[test]
    fn a_step_selects_every_nth_value() {
        assert_eq!(minutes_of("*/5 * * * *"), vec![0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55]);
        assert_eq!(minutes_of("*/20 * * * *"), vec![0, 20, 40]);
    }

    #[test]
    fn a_range_selects_its_endpoints_too() {
        assert_eq!(minutes_of("1-5 * * * *"), vec![1, 2, 3, 4, 5]);
        assert_eq!(minutes_of("0-0 * * * *"), vec![0]);
    }

    #[test]
    fn a_list_selects_each_of_its_members() {
        assert_eq!(minutes_of("1,15 * * * *"), vec![1, 15]);
        assert_eq!(minutes_of("30,0,30 * * * *"), vec![0, 30], "duplicates collapse");
    }

    #[test]
    fn a_step_can_be_applied_to_a_range_or_a_start() {
        assert_eq!(minutes_of("0-30/10 * * * *"), vec![0, 10, 20, 30]);
        assert_eq!(minutes_of("50/5 * * * *"), vec![50, 55], "`a/n` runs to the end of the field");
    }

    #[test]
    fn lists_ranges_and_steps_combine_in_one_field() {
        assert_eq!(minutes_of("0,10-12,*/30 * * * *"), vec![0, 10, 11, 12, 30]);
    }

    #[test]
    fn nonsense_expressions_are_rejected_with_an_explanation() {
        for expression in [
            "* * * *",           // four fields
            "* * * * * *",       // six fields
            "",                  // nothing at all
            "60 * * * *",        // minute out of range
            "* 24 * * *",        // hour out of range
            "* * 0 * *",         // there is no day zero
            "* * 32 * *",        // nor a thirty-second
            "* * * 13 *",        // nor a thirteenth month
            "* * * * 8",         // nor an eighth weekday
            "*/0 * * * *",       // a step of zero selects nothing
            "5-1 * * * *",       // backwards range
            "abc * * * *",       // not a number
            "*/x * * * *",       // not a step
            "1,, * * * *",       // empty list member
        ] {
            assert!(
                Cron::parse(expression).is_err(),
                "{expression:?} should have been rejected"
            );
        }
    }

    #[test]
    fn an_error_says_what_a_field_may_contain() {
        let message = Cron::parse("70 * * * *").unwrap_err().to_string();
        assert!(message.contains("minute"), "{message}");
        assert!(message.contains("between 0 and 59"), "{message}");

        let count = Cron::parse("* * *").unwrap_err().to_string();
        assert!(count.contains("needs exactly 5"), "{count}");
    }

    #[test]
    fn every_minute_advances_by_a_minute() {
        let start = to_unix(2026, 8, 29, 13, 0, 30);
        assert_eq!(next("* * * * *", start), to_unix(2026, 8, 29, 13, 1, 0));
    }

    #[test]
    fn the_next_run_is_strictly_after_the_moment_asked_about() {
        // Standing exactly on a matching minute must give the *following* one,
        // or a scheduler that asks "what is next?" would run the same tick twice.
        let on_the_hour = to_unix(2026, 8, 29, 13, 0, 0);
        assert_eq!(next("0 * * * *", on_the_hour), to_unix(2026, 8, 29, 14, 0, 0));
    }

    #[test]
    fn a_daily_schedule_lands_on_the_configured_time() {
        let morning = to_unix(2026, 8, 29, 9, 0, 0);
        assert_eq!(next("0 13 * * *", morning), to_unix(2026, 8, 29, 13, 0, 0));

        let evening = to_unix(2026, 8, 29, 18, 0, 0);
        assert_eq!(next("0 13 * * *", evening), to_unix(2026, 8, 30, 13, 0, 0));
    }

    #[test]
    fn a_five_minute_step_wraps_across_the_hour() {
        assert_eq!(
            next("*/5 * * * *", to_unix(2026, 8, 29, 13, 57, 10)),
            to_unix(2026, 8, 29, 14, 0, 0)
        );
        assert_eq!(
            next("*/5 * * * *", to_unix(2026, 8, 29, 13, 2, 0)),
            to_unix(2026, 8, 29, 13, 5, 0)
        );
    }

    #[test]
    fn a_weekday_schedule_skips_the_weekend() {
        // 29 August 2026 is a Saturday, so "weekdays at 09:00" waits for Monday.
        let saturday = to_unix(2026, 8, 29, 10, 0, 0);
        assert_eq!(next("0 9 * * 1-5", saturday), to_unix(2026, 8, 31, 9, 0, 0));
    }

    #[test]
    fn sunday_can_be_spelled_zero_or_seven() {
        let saturday = to_unix(2026, 8, 29, 10, 0, 0);
        let sunday = to_unix(2026, 8, 30, 0, 0, 0);

        assert_eq!(next("0 0 * * 0", saturday), sunday);
        assert_eq!(next("0 0 * * 7", saturday), sunday);
    }

    #[test]
    fn both_day_fields_together_mean_or_not_and() {
        // The first of the month, or any Monday.
        let cron = Cron::parse("0 0 1 * 1").unwrap();

        // 1 September 2026 is a Tuesday: matches on day of month alone.
        assert!(cron.matches(&time::from_unix(to_unix(2026, 9, 1, 0, 0, 0))));
        // 7 September 2026 is a Monday: matches on weekday alone.
        assert!(cron.matches(&time::from_unix(to_unix(2026, 9, 7, 0, 0, 0))));
        // 2 September 2026 is neither.
        assert!(!cron.matches(&time::from_unix(to_unix(2026, 9, 2, 0, 0, 0))));
    }

    #[test]
    fn a_yearly_schedule_crosses_the_year_boundary() {
        assert_eq!(
            next("0 0 1 1 *", to_unix(2026, 8, 29, 0, 0, 0)),
            to_unix(2027, 1, 1, 0, 0, 0)
        );
    }

    #[test]
    fn a_leap_day_schedule_waits_for_a_leap_year() {
        assert_eq!(
            next("0 0 29 2 *", to_unix(2026, 3, 1, 0, 0, 0)),
            to_unix(2028, 2, 29, 0, 0, 0)
        );
    }

    #[test]
    fn a_date_that_never_happens_has_no_next_run() {
        assert_eq!(Cron::parse("0 0 30 2 *").unwrap().next_after(0), None);
        assert_eq!(Cron::parse("0 0 31 4 *").unwrap().next_after(0), None);
    }

    #[test]
    fn matching_and_next_run_agree_with_each_other() {
        let cron = Cron::parse("*/7 2-4 * * *").unwrap();
        let mut at = to_unix(2026, 1, 1, 0, 0, 0);

        for _ in 0..200 {
            let run = cron.next_after(at).expect("this expression always has a next run");
            assert!(run > at);
            assert!(cron.matches(&time::from_unix(run)), "next_after returned a non-matching time");

            // Nothing between `at` and `run` may match, or the answer was late.
            let mut between = time::start_of_minute(at) + 60;
            while between < run {
                assert!(
                    !cron.matches(&time::from_unix(between)),
                    "skipped a matching minute at {between}"
                );
                between += 60;
            }
            at = run;
        }
    }

    #[test]
    fn the_expression_is_kept_in_normalised_form() {
        assert_eq!(Cron::parse("  */5   *  * * * ").unwrap().expression(), "*/5 * * * *");
    }

    #[test]
    fn weekday_numbers_follow_cron_not_iso() {
        assert_eq!(Weekday::Sunday.number(), 0);
        assert_eq!(Weekday::Monday.number(), 1);
        assert_eq!(Weekday::Saturday.number(), 6);
    }
}
