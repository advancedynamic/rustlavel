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

/// The number of characters a token has: `random::hex(32)` is 64.
pub const TOKEN_CHARS: usize = 64;

/// Why a link did not work.
///
/// Three different situations used to share one sentence — "expired or already
/// used" — and the least likely of the three was the only one that sentence
/// named. A link cut in half by an email program, and a link replaced by a
/// newer request, both sent people to check the clock. Time was never the
/// problem.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LinkProblem {
    /// Not the shape a token has, so it can never have existed. Almost always
    /// a link that arrived broken across two lines and was copied short.
    Malformed,
    /// Real, but a newer link for the same person replaced it. Asking twice is
    /// enough to do this, and the first email's link dies the moment the second
    /// is sent.
    Superseded,
    /// Used already, or past its hour. Also what an unrecognised token reports:
    /// telling somebody that a token never existed says more than it needs to.
    Spent,
}

impl LinkProblem {
    /// What to put on the page. `subject` names the kind of link, so the same
    /// three sentences serve activation, reset and email-change.
    pub fn message(self, subject: &str) -> String {
        match self {
            LinkProblem::Malformed => format!(
                "That {subject} link is incomplete. Mail programs often break a long link across \
                 two lines — check that the whole address was copied, or ask for a new one."
            ),
            LinkProblem::Superseded => format!(
                "A newer {subject} link was sent after this one, and only the newest works. Open \
                 the most recent email, or ask for a new link."
            ),
            LinkProblem::Spent => format!(
                "That {subject} link has expired or has already been used."
            ),
        }
    }
}

/// The shape a token has, checked before the database is asked.
///
/// A token of the wrong length or with a character outside hexadecimal cannot
/// match any row, so this is not an optimisation — it is the difference between
/// "this link is broken" and "this link is old", which are different problems
/// with different fixes.
pub fn is_well_formed(token: &str) -> bool {
    token.len() == TOKEN_CHARS && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Why this token did not work, for a message somebody can act on.
///
/// Only ever called once a claim has already failed, so it costs nothing on
/// the path that works.
pub async fn diagnose(db: &Database, purpose: &str, token: &str) -> Result<LinkProblem> {
    if !is_well_formed(token) {
        return Ok(LinkProblem::Malformed);
    }

    // Deliberately without the `used_at` and `expires_at` filters: the question
    // here is what became of the row, not whether it is usable.
    let found = UserToken::first(
        db,
        UserToken::query().filter("purpose", purpose).filter("token_hash", hash_token(token)),
    )
    .await?;

    let Some(record) = found else {
        // Unrecognised. Reported as spent rather than as unknown, so that
        // holding a token tells nobody whether it was ever real.
        return Ok(LinkProblem::Spent);
    };

    // A later row for the same person and purpose means `issue` retired this
    // one. Ids ascend, so later is higher.
    let newer = UserToken::first(
        db,
        UserToken::query()
            .filter("user_id", record.user_id)
            .filter("purpose", purpose)
            .filter_op("id", ">", record.id),
    )
    .await?;

    Ok(if newer.is_some() { LinkProblem::Superseded } else { LinkProblem::Spent })
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
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
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

pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month = i64::from(month);
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
