//! When the next backup is due, and whether one is now.
//!
//! Separated from the controller because it is arithmetic with no database in
//! it, which makes it the one part of the backup schedule that can be tested
//! without a server, a clock, or a cron entry.
//!
//! **A schedule here is a statement of intent, not a timer.** This application
//! has no clock of its own: something outside it has to ask "is one due?" and
//! act on the answer — the queue package's `Scheduler`, or a cron entry
//! calling the binary. The Backup tab says so, and says so *loudly* when a
//! schedule is set and nothing has ever run one. A switch that silently does
//! nothing is the failure this whole screen exists to avoid.

use crate::support::tokens;

/// How long between runs, in seconds. `None` when the schedule is off.
pub fn interval(schedule: &str) -> Option<i64> {
    match schedule {
        "6h" => Some(6 * 3600),
        "daily" => Some(24 * 3600),
        "weekly" => Some(7 * 24 * 3600),
        _ => None,
    }
}

/// A schedule as a person reads it.
pub fn describe(schedule: &str) -> &'static str {
    match schedule {
        "6h" => "every six hours",
        "daily" => "once a day",
        "weekly" => "once a week",
        _ => "never — backups are taken by hand",
    }
}

/// When the next run falls due, given the last one.
///
/// With no previous run the answer is `now`: an application that has just had
/// a schedule switched on should take one rather than wait a week to find out
/// whether backups work at all.
///
/// `now` is a parameter rather than a call to the clock, which is what makes
/// every case above testable — the first version read the clock inside and
/// could only be checked against whatever second the suite ran in.
pub fn next_due(schedule: &str, last: Option<&str>, now: &str) -> Option<String> {
    let interval = interval(schedule)?;
    match last {
        None => Some(now.to_string()),
        Some(at) => Some(tokens::format_utc(tokens::parse_utc(at) + interval)),
    }
}

/// Whether a run is owed at `now`.
pub fn is_due(schedule: &str, last: Option<&str>, now: &str) -> bool {
    match next_due(schedule, last, now) {
        None => false,
        Some(due) => tokens::parse_utc(now) >= tokens::parse_utc(&due),
    }
}

/// The backups to delete, given the ones there are and how many to keep.
///
/// Takes ids newest-first and returns the ones past the window. Zero keeps
/// everything, which is the default: silently deleting somebody's backups
/// because a number defaulted to 7 is not a thing a settings page should do
/// on the first save.
pub fn beyond_retention(newest_first: &[i64], keep: usize) -> Vec<i64> {
    if keep == 0 {
        return Vec::new();
    }
    newest_first.iter().skip(keep).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_schedule_that_has_never_run_is_due_now() {
        assert!(is_due("daily", None, "2026-09-03 10:00:00"));
        // Switching it off is not "overdue since the beginning of time".
        assert!(!is_due("disabled", None, "2026-09-03 10:00:00"));
        assert!(!is_due("", None, "2026-09-03 10:00:00"));
    }

    #[test]
    fn the_next_run_is_one_interval_after_the_last() {
        let now = "2026-09-03 06:00:00";

        assert_eq!(next_due("6h", Some(now), now).as_deref(), Some("2026-09-03 12:00:00"));
        assert_eq!(next_due("weekly", Some(now), now).as_deref(), Some("2026-09-10 06:00:00"));
        assert_eq!(next_due("disabled", Some(now), now), None);
        // Never run: due now, whenever "now" happens to be.
        assert_eq!(next_due("daily", None, now).as_deref(), Some(now));
    }

    #[test]
    fn a_run_is_owed_only_once_the_interval_has_passed() {
        let last = Some("2026-09-03 06:00:00");

        assert!(!is_due("6h", last, "2026-09-03 11:59:59"));
        assert!(is_due("6h", last, "2026-09-03 12:00:00"));
        assert!(is_due("6h", last, "2026-09-04 09:00:00"));
    }

    /// Zero keeps everything. A retention window that defaulted to deleting
    /// would delete somebody's backups the first time they saved the tab.
    #[test]
    fn retention_of_zero_deletes_nothing() {
        let ids = [9, 8, 7, 6, 5, 4, 3, 2, 1];

        assert_eq!(beyond_retention(&ids, 0), Vec::<i64>::new());
        assert_eq!(beyond_retention(&ids, 7), vec![2, 1]);
        assert_eq!(beyond_retention(&ids, 20), Vec::<i64>::new());
        assert_eq!(beyond_retention(&[], 7), Vec::<i64>::new());
    }
}
