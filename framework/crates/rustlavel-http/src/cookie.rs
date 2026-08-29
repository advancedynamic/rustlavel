//! Cookie construction and parsing.

use crate::url;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    fn as_str(self) -> &'static str {
        match self {
            SameSite::Strict => "Strict",
            SameSite::Lax => "Lax",
            SameSite::None => "None",
        }
    }
}

/// A cookie to be sent with a response.
///
/// Defaults are the safe ones — `HttpOnly`, `SameSite=Lax`, path `/` — so a
/// session cookie is hardened unless the application opts out.
#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub path: Option<String>,
    pub domain: Option<String>,
    pub max_age: Option<Duration>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<SameSite>,
    /// Set independently of `max_age` to expire a cookie in the past.
    expires_unix: Option<i64>,
}

impl Cookie {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Cookie {
            name: name.into(),
            value: value.into(),
            path: Some("/".into()),
            domain: None,
            max_age: None,
            secure: false,
            http_only: true,
            same_site: Some(SameSite::Lax),
            expires_unix: None,
        }
    }

    /// A cookie that instructs the browser to drop an existing one.
    pub fn forget(name: impl Into<String>) -> Self {
        let mut cookie = Cookie::new(name, "");
        cookie.max_age = Some(Duration::ZERO);
        cookie.expires_unix = Some(0);
        cookie
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    pub fn max_age(mut self, age: Duration) -> Self {
        self.max_age = Some(age);
        self
    }

    pub fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    pub fn http_only(mut self, http_only: bool) -> Self {
        self.http_only = http_only;
        self
    }

    pub fn same_site(mut self, same_site: SameSite) -> Self {
        self.same_site = Some(same_site);
        self
    }

    /// Render the `Set-Cookie` header value.
    pub fn to_header(&self) -> String {
        let mut out = format!("{}={}", self.name, url::encode(&self.value));

        if let Some(path) = &self.path {
            out.push_str("; Path=");
            out.push_str(path);
        }
        if let Some(domain) = &self.domain {
            out.push_str("; Domain=");
            out.push_str(domain);
        }
        if let Some(age) = self.max_age {
            out.push_str(&format!("; Max-Age={}", age.as_secs()));
        }
        if let Some(expires) = self.expires_unix {
            out.push_str(&format!("; Expires={}", http_date(expires)));
        }
        if self.secure {
            out.push_str("; Secure");
        }
        if self.http_only {
            out.push_str("; HttpOnly");
        }
        if let Some(same_site) = self.same_site {
            out.push_str("; SameSite=");
            out.push_str(same_site.as_str());
            // SameSite=None is only honoured on a Secure cookie.
            if same_site == SameSite::None && !self.secure {
                out.push_str("; Secure");
            }
        }
        out
    }
}

/// Parse a request's `Cookie` header into name/value pairs.
pub fn parse_header(header: &str) -> BTreeMap<String, String> {
    header
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .map(|(name, value)| (name.trim().to_string(), url::decode(value.trim())))
        .collect()
}

/// Format a unix timestamp as an HTTP date (RFC 7231 IMF-fixdate).
fn http_date(unix: i64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_hardened_ones() {
        let header = Cookie::new("session", "abc").to_header();

        assert!(header.starts_with("session=abc"));
        assert!(header.contains("; Path=/"));
        assert!(header.contains("; HttpOnly"));
        assert!(header.contains("; SameSite=Lax"));
    }

    #[test]
    fn same_site_none_forces_secure() {
        let header = Cookie::new("x", "1").same_site(SameSite::None).to_header();
        assert!(header.contains("; Secure"));
    }

    #[test]
    fn values_are_encoded_and_decoded() {
        let header = Cookie::new("greeting", "hello world").to_header();
        assert!(header.starts_with("greeting=hello%20world"));

        let parsed = parse_header("greeting=hello%20world; other=2");
        assert_eq!(parsed["greeting"], "hello world");
        assert_eq!(parsed["other"], "2");
    }

    #[test]
    fn forget_expires_in_the_past() {
        let header = Cookie::forget("session").to_header();
        assert!(header.contains("Max-Age=0"));
        assert!(header.contains("Expires=Thu, 01 Jan 1970 00:00:00 GMT"));
    }

    #[test]
    fn formats_known_http_dates() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_000_000_000), "Sun, 09 Sep 2001 01:46:40 GMT");
    }
}
