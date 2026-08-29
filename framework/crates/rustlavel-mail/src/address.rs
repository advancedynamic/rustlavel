//! Email addresses, and the display names attached to them.

use crate::encode::{encode_word, sanitize_header_text};
use rustlavel_core::{Error, Result};
use std::fmt;

/// The characters RFC 5322 calls "specials". A display name containing one of
/// them has to be quoted, or the parser downstream reads it as syntax.
const SPECIALS: &[char] = &['(', ')', '<', '>', '@', ',', ';', ':', '\\', '"', '[', ']', '.'];

/// One mailbox: an address, and optionally the name a human should see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    name: Option<String>,
    email: String,
}

impl Address {
    /// A bare address, validated.
    pub fn new(email: impl Into<String>) -> Result<Address> {
        let email = email.into().trim().to_string();
        validate(&email)?;
        Ok(Address { name: None, email })
    }

    /// An address with the name a recipient's client will show.
    pub fn named(name: impl Into<String>, email: impl Into<String>) -> Result<Address> {
        let mut address = Address::new(email)?;
        let name = sanitize_header_text(&name.into());
        address.name = (!name.is_empty()).then_some(name);
        Ok(address)
    }

    /// Parse `Ada Lovelace <ada@example.com>`, `<ada@example.com>`, or a bare
    /// `ada@example.com` — the three shapes a configuration file or a header
    /// actually contains.
    pub fn parse(input: &str) -> Result<Address> {
        let input = input.trim();

        let Some(open) = input.rfind('<') else {
            return Address::new(input);
        };
        let close = input[open..].find('>').map(|at| open + at).ok_or_else(|| {
            Error::msg(format!("`{input}` is missing the closing `>` around the address"))
        })?;

        let email = &input[open + 1..close];
        let name = input[..open].trim().trim_matches('"');

        if name.is_empty() { Address::new(email) } else { Address::named(name, email) }
    }

    pub fn email(&self) -> &str {
        &self.email
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The domain part, which is what a `Message-ID` is built from.
    pub fn domain(&self) -> &str {
        self.email.split_once('@').map(|(_, domain)| domain).unwrap_or("localhost")
    }

    /// How this address is written in a header: the name encoded or quoted as
    /// it needs to be, the address inside angle brackets.
    pub fn to_header(&self) -> String {
        match &self.name {
            None => self.email.clone(),
            Some(name) => format!("{} <{}>", encode_display_name(name), self.email),
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_header())
    }
}

/// Anything a caller may hand to `.to(...)`.
///
/// Conversion is fallible, but the builder keeps the error rather than
/// returning it, so a chain of `.to(...).cc(...)` stays a chain and the failure
/// surfaces once, at send time, naming the address that was wrong.
pub trait IntoAddress {
    fn into_address(self) -> Result<Address>;
}

impl IntoAddress for Address {
    fn into_address(self) -> Result<Address> {
        Ok(self)
    }
}

impl IntoAddress for &Address {
    fn into_address(self) -> Result<Address> {
        Ok(self.clone())
    }
}

impl IntoAddress for &str {
    fn into_address(self) -> Result<Address> {
        Address::parse(self)
    }
}

impl IntoAddress for String {
    fn into_address(self) -> Result<Address> {
        Address::parse(&self)
    }
}

/// `("Ada Lovelace", "ada@example.com")`.
impl IntoAddress for (&str, &str) {
    fn into_address(self) -> Result<Address> {
        Address::named(self.0, self.1)
    }
}

/// Quote or encode a display name so it survives as one token.
fn encode_display_name(name: &str) -> String {
    if !name.is_ascii() {
        // An encoded-word is never quoted: quotes around it would be shown
        // literally by every client that decodes it.
        return encode_word(name);
    }
    if name.contains(SPECIALS) {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("\"{escaped}\"");
    }
    name.to_string()
}

/// Reject anything that is not an address a server will accept.
///
/// Strict on purpose. This value ends up in a `RCPT TO:<...>` command, so a
/// space or a newline inside it is not a typo, it is a way to inject SMTP.
fn validate(email: &str) -> Result<()> {
    if email.is_empty() {
        return Err(Error::msg("an email address cannot be empty"));
    }
    if email.len() > 320 {
        return Err(Error::msg(format!(
            "`{email}` is longer than the 320 characters an address may have"
        )));
    }
    if email.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::msg(format!(
            "`{email}` contains whitespace or a control character — an address may contain neither"
        )));
    }

    let Some((local, domain)) = email.split_once('@') else {
        return Err(Error::msg(format!("`{email}` is not an email address — it has no `@`")));
    };
    if local.is_empty() {
        return Err(Error::msg(format!("`{email}` has nothing before the `@`")));
    }
    if domain.is_empty() {
        return Err(Error::msg(format!("`{email}` has no domain after the `@`")));
    }
    if domain.contains('@') {
        return Err(Error::msg(format!("`{email}` has more than one `@`")));
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return Err(Error::msg(format!("`{email}` has a malformed domain")));
    }

    Ok(())
}

/// Render a list of addresses as one header value.
pub fn address_list(addresses: &[Address]) -> String {
    addresses.iter().map(Address::to_header).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_address_needs_no_name() {
        let address = Address::new("ada@example.com").unwrap();

        assert_eq!(address.to_header(), "ada@example.com");
        assert_eq!(address.domain(), "example.com");
        assert_eq!(address.name(), None);
    }

    #[test]
    fn a_display_name_is_written_before_the_angle_brackets() {
        let address = Address::named("Ada Lovelace", "ada@example.com").unwrap();
        assert_eq!(address.to_header(), "Ada Lovelace <ada@example.com>");
    }

    #[test]
    fn a_name_containing_specials_is_quoted() {
        let address = Address::named("Lovelace, Ada", "ada@example.com").unwrap();
        assert_eq!(address.to_header(), "\"Lovelace, Ada\" <ada@example.com>");

        let awkward = Address::named("Ada \"The Countess\"", "ada@example.com").unwrap();
        assert_eq!(
            awkward.to_header(),
            "\"Ada \\\"The Countess\\\"\" <ada@example.com>"
        );
    }

    #[test]
    fn a_non_ascii_name_becomes_an_encoded_word_and_is_not_quoted() {
        let address = Address::named("Bj\u{f6}rn Str\u{f6}m", "bjorn@example.com").unwrap();

        assert_eq!(address.to_header(), "=?UTF-8?B?QmrDtnJuIFN0csO2bQ==?= <bjorn@example.com>");
        assert!(!address.to_header().contains('"'));
    }

    #[test]
    fn addresses_parse_out_of_the_three_shapes_that_occur_in_practice() {
        assert_eq!(Address::parse("ada@example.com").unwrap().email(), "ada@example.com");
        assert_eq!(Address::parse("<ada@example.com>").unwrap().email(), "ada@example.com");

        let named = Address::parse("Ada Lovelace <ada@example.com>").unwrap();
        assert_eq!((named.name(), named.email()), (Some("Ada Lovelace"), "ada@example.com"));

        let quoted = Address::parse("\"Lovelace, Ada\" <ada@example.com>").unwrap();
        assert_eq!(quoted.name(), Some("Lovelace, Ada"));
    }

    #[test]
    fn a_malformed_address_is_refused_with_a_reason() {
        for bad in ["", "no-at-sign", "@example.com", "ada@", "ada@@example.com", "a b@x.com"] {
            let error = Address::new(bad).unwrap_err().to_string();
            assert!(!error.is_empty(), "`{bad}` should have been refused");
        }

        let error = Address::new("no-at-sign").unwrap_err().to_string();
        assert!(error.contains("no `@`"), "{error}");
    }

    #[test]
    fn an_address_cannot_carry_a_newline_into_a_smtp_command() {
        // `RCPT TO:<...>` is built from this value; a CRLF in it would be a new
        // command line, which is the SMTP version of header injection.
        let error = Address::new("ada@example.com\r\nRCPT TO:<evil@example.com>").unwrap_err();
        assert!(error.to_string().contains("whitespace"), "{error}");
    }

    #[test]
    fn a_newline_in_a_display_name_is_flattened_rather_than_folded() {
        let address = Address::named("Ada\r\nBcc: evil@example.com", "ada@example.com").unwrap();

        assert!(!address.to_header().contains('\n'));
        assert!(address.to_header().starts_with("\"Ada Bcc"));
    }

    #[test]
    fn a_list_of_addresses_is_comma_separated() {
        let list = address_list(&[
            Address::named("Ada", "ada@example.com").unwrap(),
            Address::new("grace@example.com").unwrap(),
        ]);

        assert_eq!(list, "Ada <ada@example.com>, grace@example.com");
    }

    #[test]
    fn the_conversion_trait_accepts_the_shapes_a_caller_reaches_for() {
        assert_eq!("ada@example.com".into_address().unwrap().email(), "ada@example.com");
        assert_eq!(
            ("Ada", "ada@example.com").into_address().unwrap().to_header(),
            "Ada <ada@example.com>"
        );
        assert_eq!(
            String::from("Ada <ada@example.com>").into_address().unwrap().email(),
            "ada@example.com"
        );
    }
}
