//! Redirect URI matching, which is where authorisation servers get breached.
//!
//! The authorisation code is delivered by sending the user's browser to the
//! `redirect_uri` the request asked for. If a server can be talked into sending
//! it somewhere the client did not register, the attacker receives the code and
//! the flow completes as them: a full account takeover, from one query
//! parameter. It is the most commonly botched part of an authorisation server,
//! and every botch has the same shape — a comparison that is *almost* exact.
//!
//! So the rule here is the whole of RFC 6749 §3.1.2.3 and the OAuth 2.1 draft's
//! §4.1.1, and nothing more: **byte-for-byte string equality against the
//! registered set.** No prefix matching, no wildcards, no normalising a
//! trailing slash, no ignoring the query string, no case folding the host.
//! Every one of those relaxations is a known bypass:
//!
//! | Relaxation                  | What it lets through                     |
//! |-----------------------------|------------------------------------------|
//! | prefix match                | `https://a.test/cb.evil.com`             |
//! | ignore the query string     | `https://a.test/cb?next=//evil.com`      |
//! | normalise `..`              | `https://a.test/cb/../open-redirect`     |
//! | wildcard subdomains         | `https://evil.a.test/cb`                 |
//! | case-fold the path          | a path that routes differently upstream  |
//!
//! Comparing the raw strings is not a shortcut past those cases; it is the only
//! comparison that has none of them.

/// Whether `presented` is exactly one of the URIs the client registered.
///
/// The entire implementation is `==`, on purpose. If this function ever grows a
/// branch, the branch is the vulnerability.
pub fn is_registered(registered: &[String], presented: &str) -> bool {
    registered.iter().any(|uri| uri == presented)
}

/// Whether a URI is fit to be *registered*, checked once when a client is added.
///
/// This is the only place any judgement is applied to a redirect URI, and it is
/// applied at registration rather than at authorisation time so a
/// misconfiguration fails at boot with a sentence explaining it, instead of
/// half a year later as a redirect that quietly works.
///
/// Three rules, each from the specification:
///
/// * absolute, with a scheme and (for `http`/`https`) a host — RFC 6749 §3.1.2;
/// * no fragment, since the fragment is never sent to the server and a
///   registered one could never match what arrives — RFC 6749 §3.1.2;
/// * `https`, unless it is loopback — OAuth 2.1 §4.1.1. A native application
///   redirecting to `http://127.0.0.1:1234/cb` is the documented exception;
///   `http://example.com/cb` puts the code on the wire in clear text.
///
/// A private-use scheme (`com.example.app:/cb`) is allowed, because that is how
/// a mobile client receives the redirect at all.
pub fn validate_registration(uri: &str) -> Result<(), String> {
    let Some((scheme, rest)) = uri.split_once(':') else {
        return Err(format!(
            "a redirect URI must be absolute, with a scheme: {uri:?} has none (RFC 6749 §3.1.2)"
        ));
    };

    if scheme.is_empty() || !scheme.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Err(format!("{uri:?} does not start with a URI scheme (RFC 6749 §3.1.2)"));
    }

    if uri.contains('#') {
        return Err(format!(
            "a redirect URI may not carry a fragment: {uri:?}. The browser never sends the \
             fragment to the server, so a registered one could never be matched (RFC 6749 §3.1.2)"
        ));
    }

    if scheme.eq_ignore_ascii_case("http") && !is_loopback(rest) {
        return Err(format!(
            "{uri:?} is plain http, which puts the authorization code on the wire in clear \
             text. OAuth 2.1 §4.1.1 allows http only for loopback — http://127.0.0.1, \
             http://[::1] or http://localhost — which is how a native client receives a \
             redirect. Register the https URL instead."
        ));
    }

    if (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && host_of(rest).is_empty()
    {
        return Err(format!("{uri:?} has no host (RFC 6749 §3.1.2)"));
    }

    Ok(())
}

/// The host of the `//host:port/path` remainder of an http(s) URI.
fn host_of(rest: &str) -> &str {
    // Exactly two slashes: `https:///cb` has no authority at all, and stripping
    // every leading slash would read its path as the host.
    let Some(authority) = rest.strip_prefix("//") else { return "" };
    let authority = authority.split(['/', '?']).next().unwrap_or("");
    // Strip userinfo, then the port. A bracketed IPv6 literal keeps its colons.
    let authority = authority.rsplit('@').next().unwrap_or("");
    match authority.strip_prefix('[') {
        Some(inner) => inner.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    }
}

/// Whether an `http` URI points at this machine.
///
/// `localhost` is included because that is what the OAuth 2.1 draft names, even
/// though it resolves through the host's resolver rather than being an address.
fn is_loopback(rest: &str) -> bool {
    let host = host_of(rest);
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered() -> Vec<String> {
        vec!["https://a.test/cb".to_string()]
    }

    #[test]
    fn the_registered_uri_matches_itself_and_nothing_else_does() {
        assert!(is_registered(&registered(), "https://a.test/cb"));
    }

    #[test]
    fn a_trailing_slash_is_a_different_uri() {
        // `/cb/` can route to a different handler, and on plenty of servers to
        // a directory listing. It was not registered, so it does not match.
        assert!(!is_registered(&registered(), "https://a.test/cb/"));
    }

    #[test]
    fn an_added_query_string_is_a_different_uri() {
        // The classic parameter-injection bypass: the path matches, and the
        // query carries the attacker's payload into the client's own redirect.
        assert!(!is_registered(&registered(), "https://a.test/cb?x=1"));
        assert!(!is_registered(&registered(), "https://a.test/cb?"));
    }

    #[test]
    fn a_dot_dot_segment_is_not_normalised_away() {
        // Only true because nothing normalises: `https://a.test/cb/../evil`
        // resolves in the browser to `https://a.test/evil`.
        assert!(!is_registered(&registered(), "https://a.test/cb/../evil"));
        assert!(!is_registered(&registered(), "https://a.test/./cb"));
    }

    #[test]
    fn a_suffixed_host_is_not_the_registered_host() {
        // What a prefix match would let through, and the reason there is none.
        assert!(!is_registered(&registered(), "https://a.test.evil.com/cb"));
        assert!(!is_registered(&registered(), "https://a.test@evil.com/cb"));
        assert!(!is_registered(&registered(), "https://evil.com/?x=https://a.test/cb"));
    }

    #[test]
    fn a_scheme_relative_uri_is_not_the_registered_one() {
        // `//evil.com` inherits the current scheme in a browser and is a
        // perfectly ordinary redirect to somebody else's site.
        assert!(!is_registered(&registered(), "//evil.com"));
        assert!(!is_registered(&registered(), "//a.test/cb"));
    }

    #[test]
    fn the_scheme_port_and_case_all_have_to_agree() {
        assert!(!is_registered(&registered(), "http://a.test/cb"), "scheme");
        assert!(!is_registered(&registered(), "https://a.test:443/cb"), "port");
        assert!(!is_registered(&registered(), "https://A.TEST/cb"), "host case");
        assert!(!is_registered(&registered(), "https://a.test/CB"), "path case");
    }

    #[test]
    fn an_empty_registration_matches_nothing_at_all() {
        // A client with no registered URI must not be usable in a code flow,
        // rather than accidentally accepting the empty string.
        assert!(!is_registered(&[], "https://a.test/cb"));
        assert!(!is_registered(&[], ""));
    }

    #[test]
    fn several_registered_uris_each_match_only_themselves() {
        let registered =
            vec!["https://a.test/cb".to_string(), "https://a.test/other".to_string()];

        assert!(is_registered(&registered, "https://a.test/other"));
        assert!(!is_registered(&registered, "https://a.test/oth"));
    }

    #[test]
    fn registration_accepts_https_loopback_and_private_use_schemes() {
        assert!(validate_registration("https://a.test/cb").is_ok());
        assert!(validate_registration("https://a.test:8443/cb?x=1").is_ok());
        assert!(validate_registration("http://127.0.0.1:1234/cb").is_ok());
        assert!(validate_registration("http://localhost/cb").is_ok());
        assert!(validate_registration("http://[::1]:9000/cb").is_ok());
        assert!(validate_registration("com.example.app:/oauth").is_ok());
    }

    #[test]
    fn registration_refuses_plain_http_to_anywhere_but_loopback() {
        let error = validate_registration("http://example.com/cb").unwrap_err();
        assert!(error.contains("clear text"), "got {error}");

        // Not loopback, whatever it is called locally.
        assert!(validate_registration("http://127.0.0.1.evil.com/cb").is_err());
        assert!(validate_registration("http://localhost.evil.com/cb").is_err());
    }

    #[test]
    fn registration_refuses_a_relative_uri_or_a_fragment() {
        assert!(validate_registration("/cb").is_err());
        assert!(validate_registration("a.test/cb").is_err());
        assert!(validate_registration("//evil.com").is_err());

        let fragment = validate_registration("https://a.test/cb#done").unwrap_err();
        assert!(fragment.contains("fragment"), "got {fragment}");
    }

    #[test]
    fn registration_refuses_a_hostless_http_uri() {
        assert!(validate_registration("https:///cb").is_err());
        assert!(validate_registration("https://").is_err());
    }
}
