//! Emailed single-use links, and the small amount of time arithmetic the kit
//! needs.
//!
//! Times are stored and compared as `YYYY-MM-DD HH:MM:SS` in UTC. Strings
//! rather than a date type because they sort correctly as text, every one of
//! the three databases stores them the same way, and the comparison a lock
//! needs is `expires_at > now`, which is a string comparison.

use rustlavel::prelude::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::models::user_token::{UserToken, hash_token};

/// How long an activation or reset link is good for.
pub const LINK_LIFETIME: Duration = Duration::from_secs(60 * 60);

pub fn now() -> String {
    format_utc(unix_now())
}

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub fn in_future(from: &str, ahead: Duration) -> String {
    format_utc(parse_utc(from) + ahead.as_secs() as i64)
}

pub fn minutes_between(from: &str, to: &str) -> i64 {
    ((parse_utc(to) - parse_utc(from)).max(0)) / 60
}

/// `YYYY-MM-DD HH:MM:SS` from a unix timestamp.
pub fn format_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// The inverse. An unparseable string reads as the epoch, which makes a
/// malformed timestamp look expired rather than look valid.
pub fn parse_utc(text: &str) -> i64 {
    let mut parts = text.split(['-', ' ', ':', 'T']);
    let mut next = || parts.next().and_then(|p| p.parse::<i64>().ok()).unwrap_or(0);
    let (year, month, day) = (next(), next(), next());
    let (hour, minute, second) = (next(), next(), next());
    if year == 0 {
        return 0;
    }
    days_from_civil(year, month.max(1) as u32, day.max(1) as u32) * 86_400
        + hour * 3600
        + minute * 60
        + second
}

/// A friendly form for a template: `2 Sep 2026 at 14:05`.
pub fn humanise(text: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let unix = parse_utc(text);
    if unix == 0 {
        return "—".to_string();
    }
    let (year, month, day) = civil_from_days(unix.div_euclid(86_400));
    let seconds = unix.rem_euclid(86_400);
    format!(
        "{day} {} {year} at {:02}:{:02}",
        MONTHS[(month - 1) as usize],
        seconds / 3600,
        (seconds % 3600) / 60
    )
}

/// Issue a link. Returns the plaintext token, which exists only in the email.
///
/// Any earlier token for the same purpose is spent first: a person who clicks
/// "resend" three times should not end up with three working links, and the
/// one in the newest email should be the one that works.
pub async fn issue(
    db: &Database,
    user_id: i64,
    purpose: &str,
    payload: Option<String>,
) -> Result<String> {
    let now = now();
    db.table("user_tokens")
        .filter("user_id", user_id)
        .filter("purpose", purpose)
        .filter_null("used_at")
        .update(db, &[("used_at", now.clone().into())])
        .await?;

    // 32 bytes from the OS CSPRNG. There is nothing to guess and nothing to
    // rate-limit if the token is this size; the whole security of the link
    // rests on this line.
    let token = rustlavel::auth::random::hex(32);

    let mut record = UserToken {
        user_id,
        purpose: purpose.to_string(),
        token_hash: hash_token(&token),
        payload,
        expires_at: in_future(&now, LINK_LIFETIME),
        ..Default::default()
    };
    record.insert(db).await?;
    Ok(token)
}

/// Look a token up and spend it, in that order.
///
/// Spending it before the caller acts is deliberate: two clicks arriving
/// together must not both succeed, and a reset that half-failed should leave
/// the link dead rather than reusable.
pub async fn claim(db: &Database, purpose: &str, token: &str) -> Result<Option<UserToken>> {
    let now = now();
    let Some(record) = UserToken::first(db, UserToken::usable(purpose, token, &now)).await? else {
        return Ok(None);
    };

    let spent = db
        .table("user_tokens")
        .filter("id", record.id)
        .filter_null("used_at")
        .update(db, &[("used_at", now.into())])
        .await?;

    // Zero rows means somebody else spent it between the read and the write.
    Ok((spent == 1).then_some(record))
}

/// Howard Hinnant's civil calendar, the same pair the HTTP date code uses.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (year + i64::from(month <= 2), month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
