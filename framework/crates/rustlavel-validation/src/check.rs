//! The hand-written format checks the rules are built on.
//!
//! Laravel leans on PCRE for `email`, `url`, `uuid` and friends. Rustlavel has
//! no regex engine and does not want one, so each shape is a small function
//! here instead. They are deliberately *pragmatic, not RFC-exhaustive*: the job
//! is to reject the typos a user actually makes in a form, not to accept every
//! address RFC 5322 permits. Each function documents where it draws that line.

/// A practical email shape: `local@domain`, one `@`, a domain with a real TLD.
///
/// Rejected on purpose even though RFC 5322 allows them: quoted local parts
/// (`"a b"@x.com`), comments, and bare hosts without a dot (`root@localhost`).
/// Anyone typing an address into a form is not typing one of those, and every
/// one of them is far more likely to be a mistake than an intent.
pub fn is_email(value: &str) -> bool {
    // The SMTP path limit; also stops a pathological input from being scanned.
    if value.len() > 254 {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    // A second `@` lands in `domain`, so checking there catches `a@b@c`.
    !domain.contains('@') && is_email_local(local) && is_domain(domain)
}

fn is_email_local(local: &str) -> bool {
    if local.is_empty() || local.len() > 64 {
        return false;
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return false;
    }
    local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~.".contains(c))
}

/// A dotted host with at least two labels and an alphabetic TLD.
fn is_domain(domain: &str) -> bool {
    if domain.is_empty() || domain.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let well_formed = labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    });
    let tld = labels[labels.len() - 1];
    well_formed && tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
}

/// A URL with an explicit scheme and a non-empty host: `https://example.com/x`.
///
/// The scheme is not restricted to http/https — `ftp://` and `redis://` are
/// URLs too — but `://` is required, so a bare `example.com` is rejected. That
/// is the mistake worth catching; a scheme allowlist belongs to the
/// application, not to a generic `url` rule.
pub fn is_url(value: &str) -> bool {
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return false;
    }

    // The authority runs up to the first path, query, or fragment delimiter.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = strip_port(host);
    !host.is_empty()
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '[' | ']'))
}

/// Drop a `:8080` suffix, leaving a bracketed IPv6 literal intact.
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host;
    }
    match host.rsplit_once(':') {
        Some((head, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => host,
    }
}

/// A calendar date in `YYYY-MM-DD`, the format an `<input type="date">` sends.
///
/// The day is checked against the month, leap years included, so `2023-02-30`
/// fails rather than silently becoming March.
pub fn is_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let (Some(year), Some(month), Some(day)) =
        (digits(&value[0..4]), digits(&value[5..7]), digits(&value[8..10]))
    else {
        return false;
    };
    (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)
}

/// Parse a run of ASCII digits. `str::parse` alone would accept `+12`.
fn digits(part: &str) -> Option<u32> {
    if part.bytes().all(|b| b.is_ascii_digit()) { part.parse().ok() } else { None }
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// The `8-4-4-4-12` hex shape of a UUID.
///
/// The version and variant nibbles are not checked: a value that came out of
/// another system may be a v1, v4, v7, or the nil UUID, and rejecting one of
/// those as "not a UUID" would be wrong.
pub fn is_uuid(value: &str) -> bool {
    let mut groups = value.split('-');
    for length in [8, 4, 4, 4, 12] {
        match groups.next() {
            Some(group)
                if group.len() == length && group.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    groups.next().is_none()
}

/// Letters only. Unicode-aware, like Laravel's default: `Ada` and `Zoë` both pass.
pub fn is_alpha(value: &str) -> bool {
    !value.is_empty() && value.chars().all(char::is_alphabetic)
}

/// Letters and digits only.
pub fn is_alpha_num(value: &str) -> bool {
    !value.is_empty() && value.chars().all(char::is_alphanumeric)
}

/// Letters, digits, dashes and underscores — the shape of a URL slug.
pub fn is_alpha_dash(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_email_addresses() {
        for address in ["ada@example.com", "a.b+tag@sub.example.co.uk", "x_1@a-b.io"] {
            assert!(is_email(address), "{address} should be a valid email");
        }
    }

    #[test]
    fn rejects_email_addresses_people_actually_mistype() {
        for address in [
            "",
            "ada",
            "ada@",
            "@example.com",
            "ada@example",
            "ada@@example.com",
            "ada b@example.com",
            "ada@example..com",
            ".ada@example.com",
            "ada.@example.com",
            "ada@-example.com",
            "ada@example.c",
            "ada@example.c0m",
        ] {
            assert!(!is_email(address), "{address} should not be a valid email");
        }
    }

    #[test]
    fn an_over_long_address_is_rejected_without_scanning_it() {
        let address = format!("{}@example.com", "a".repeat(300));
        assert!(!is_email(&address));
    }

    #[test]
    fn accepts_urls_with_a_scheme_and_a_host() {
        for url in [
            "https://example.com",
            "http://example.com/path?q=1#top",
            "https://user:pw@example.com:8443/x",
            "ftp://files.example.org",
            "http://localhost:3000",
            "http://[::1]:8080/",
        ] {
            assert!(is_url(url), "{url} should be a valid url");
        }
    }

    #[test]
    fn rejects_urls_without_a_scheme_or_host() {
        for url in ["example.com", "https://", "://example.com", "1http://x.com", "http://ex ample.com"] {
            assert!(!is_url(url), "{url} should not be a valid url");
        }
    }

    #[test]
    fn accepts_real_calendar_dates_and_rejects_impossible_ones() {
        assert!(is_date("2024-02-29"));
        assert!(is_date("1999-12-31"));

        assert!(!is_date("2023-02-29"), "2023 is not a leap year");
        assert!(!is_date("1900-02-29"), "a century that is not divisible by 400 is not a leap year");
        assert!(!is_date("2024-13-01"));
        assert!(!is_date("2024-04-31"));
        assert!(!is_date("2024-1-1"), "the format is zero padded");
        assert!(!is_date("24-01-01"));
        assert!(!is_date("2024/01/01"));
        assert!(!is_date("+024-01-01"));
    }

    #[test]
    fn recognises_uuids_of_any_version() {
        assert!(is_uuid("00000000-0000-0000-0000-000000000000"));
        assert!(is_uuid("9f8b2c1a-4d3e-4f5a-8b7c-1d2e3f4a5b6c"));
        assert!(is_uuid("9F8B2C1A-4D3E-4F5A-8B7C-1D2E3F4A5B6C"));

        assert!(!is_uuid("9f8b2c1a4d3e4f5a8b7c1d2e3f4a5b6c"));
        assert!(!is_uuid("9f8b2c1a-4d3e-4f5a-8b7c-1d2e3f4a5b6"));
        assert!(!is_uuid("9f8b2c1a-4d3e-4f5a-8b7c-1d2e3f4a5b6c-extra"));
        assert!(!is_uuid("zf8b2c1a-4d3e-4f5a-8b7c-1d2e3f4a5b6c"));
    }

    #[test]
    fn alpha_families_agree_on_the_empty_string() {
        assert!(!is_alpha(""));
        assert!(!is_alpha_num(""));
        assert!(!is_alpha_dash(""));
    }

    #[test]
    fn alpha_families_widen_one_character_class_at_a_time() {
        assert!(is_alpha("Zoë"));
        assert!(!is_alpha("Zoe2"));

        assert!(is_alpha_num("Zoe2"));
        assert!(!is_alpha_num("zoe-2"));

        assert!(is_alpha_dash("zoe-2_x"));
        assert!(!is_alpha_dash("zoe 2"));
    }
}
