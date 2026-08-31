//! The part an application actually calls.
//!
//! Almost everything anyone wants from LDAP is one question: *is this the right
//! password for this username?* Answering it correctly takes three steps that
//! are easy to get subtly wrong, so they are one call here:
//!
//! 1. Bind as a service account, because an anonymous client usually cannot
//!    read the attribute the username is stored in.
//! 2. Search for the entry whose username attribute equals what was typed. The
//!    username is a *value* in a typed [`Filter`], never text pasted into one.
//! 3. Bind as that entry's DN with the password that was typed — on a fresh
//!    connection, because a bind changes who a connection is, and a failed one
//!    leaves it anonymous rather than closed.
//!
//! ```ignore
//! use rustlavel_ldap::prelude::*;
//!
//! let directory = Directory::new("ldaps://dc.example.test")?
//!     .service_account("cn=svc,dc=example,dc=test", &config.get("LDAP_PASSWORD")?)
//!     .base_dn("ou=people,dc=example,dc=test")
//!     .username_attribute("uid")
//!     .attributes(["cn", "mail"]);
//!
//! match directory.authenticate(&form.username, &form.password).await {
//!     Ok(user) => log_in(user.dn()),
//!     Err(error) if error.is_credential_failure() => render_the_same_message_for_all_of_these(),
//!     Err(error) => return Err(error.into()),
//! }
//! ```
//!
//! # Distinguishable failures, indistinguishable messages
//!
//! [`AuthenticationError`] separates "no such user" from "wrong password" from
//! "the directory is down", because a log that cannot tell them apart is a log
//! that cannot tell you your directory has been unreachable for an hour.
//!
//! The person at the login form must not be told which one it was. A form that
//! says "no such user" for one name and "wrong password" for another is an
//! account enumeration oracle. [`AuthenticationError::is_credential_failure`]
//! exists to make the right thing easy: log the variant, show one message for
//! everything it returns true for.

use crate::connection::{LdapConfig, LdapConnection};
use crate::protocol::{
    Attribute, Filter, LdapResult, ResultCode, Scope, SearchEntry, SearchRequest,
};
use rustlavel_auth::Authenticatable;
use rustlavel_core::{Error, Result};
use std::time::Duration;

/// How the client authenticates itself before searching.
///
/// **No `Debug`.** It holds the service account's password.
enum ServiceIdentity {
    /// Search without binding at all. Legitimate — plenty of directories allow
    /// an anonymous read of `uid` — and asked for by name so it cannot be
    /// reached by leaving a password out of a configuration file.
    Anonymous,
    Simple { dn: String, password: String },
}

/// A directory, configured, ready to be asked about a user.
///
/// **No `Debug`.** It holds the service account's password.
pub struct Directory {
    config: LdapConfig,
    service: ServiceIdentity,
    base_dn: String,
    username_attribute: String,
    scope: Scope,
    extra_filter: Option<Filter>,
    attributes: Vec<String>,
    dn_template: Option<String>,
}

impl Directory {
    /// A directory at an `ldap://` or `ldaps://` URL.
    pub fn new(url: &str) -> Result<Directory> {
        Ok(Directory::from_config(LdapConfig::parse(url)?))
    }

    pub fn from_config(config: LdapConfig) -> Directory {
        Directory {
            config,
            service: ServiceIdentity::Anonymous,
            base_dn: String::new(),
            // The overwhelmingly common two: `uid` on anything derived from
            // OpenLDAP, `sAMAccountName` on Active Directory.
            username_attribute: "uid".to_string(),
            scope: Scope::Subtree,
            extra_filter: None,
            attributes: Vec::new(),
            dn_template: None,
        }
    }

    /// Bind as this account before searching.
    pub fn service_account(
        mut self,
        dn: impl Into<String>,
        password: impl Into<String>,
    ) -> Directory {
        self.service = ServiceIdentity::Simple { dn: dn.into(), password: password.into() };
        self
    }

    /// Search without binding first.
    pub fn anonymous_search(mut self) -> Directory {
        self.service = ServiceIdentity::Anonymous;
        self
    }

    /// Where in the tree to look for people.
    pub fn base_dn(mut self, dn: impl Into<String>) -> Directory {
        self.base_dn = dn.into();
        self
    }

    /// The attribute a username is stored in — `uid`, `sAMAccountName`,
    /// `userPrincipalName`, `mail`.
    pub fn username_attribute(mut self, attribute: impl Into<String>) -> Directory {
        self.username_attribute = attribute.into();
        self
    }

    pub fn scope(mut self, scope: Scope) -> Directory {
        self.scope = scope;
        self
    }

    /// An extra condition every user must also satisfy.
    ///
    /// A [`Filter`] rather than a string, because a string would be a filter
    /// template, and a filter template is the thing this design exists to avoid.
    /// `Filter::equals("objectClass", "inetOrgPerson")` is the usual one.
    pub fn filter(mut self, filter: Filter) -> Directory {
        self.extra_filter = Some(filter);
        self
    }

    /// Which attributes to fetch for an authenticated user. Empty means all of
    /// them, which is rarely what you want out of a directory.
    pub fn attributes<S: Into<String>>(
        mut self,
        attributes: impl IntoIterator<Item = S>,
    ) -> Directory {
        self.attributes = attributes.into_iter().map(Into::into).collect();
        self
    }

    /// Skip the search and bind against a DN built from the username.
    ///
    /// `{username}` is replaced with the typed name, escaped per RFC 4514. This
    /// is how a lot of Active Directory deployments are set up
    /// (`{username}@example.test`), and it needs no service account at all.
    ///
    /// The trade is real and worth stating: without a search there is no
    /// distinction between "no such user" and "wrong password" — the directory
    /// answers both with `invalidCredentials` — and no attributes come back
    /// unless [`Directory::authenticate`] is followed by a lookup. Prefer the
    /// search when you can read the directory at all.
    pub fn user_dn_template(mut self, template: impl Into<String>) -> Directory {
        self.dn_template = Some(template.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Directory {
        self.config.timeout = timeout;
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Directory {
        self.config.connect_timeout = timeout;
        self
    }

    /// Send a simple bind even though the connection is not encrypted. See
    /// [`LdapConfig::allow_plaintext_password`].
    pub fn allow_plaintext_password(mut self) -> Directory {
        self.config.allow_plaintext_password = true;
        self
    }

    /// See [`LdapConfig::dangerously_accept_any_certificate`].
    pub fn dangerously_accept_any_certificate(mut self) -> Directory {
        self.config.verify_certificate = false;
        self
    }

    pub fn config(&self) -> &LdapConfig {
        &self.config
    }

    /// Is this the right password for this username?
    ///
    /// The empty-password and empty-username checks come first, before any I/O,
    /// so a blank form field can never be confused with a directory problem —
    /// and so the answer for a blank password is the same whether the directory
    /// is up or down.
    pub async fn authenticate(
        &self,
        username: &str,
        password: &str,
    ) -> std::result::Result<User, AuthenticationError> {
        // A simple bind with a zero-length password is an *anonymous* bind, and
        // a directory answers it with `success`. A login form that passed a
        // blank password straight through would therefore report a successful
        // login for any username that exists. This is a real CVE pattern, and
        // the guard is here as well as in the encoder because here is where it
        // can be answered without touching the network.
        if password.is_empty() {
            return Err(AuthenticationError::EmptyPassword);
        }
        if username.is_empty() {
            return Err(AuthenticationError::EmptyUsername);
        }
        // A NUL cannot appear in a DN or an attribute value, and a username
        // containing one is not a username anybody typed.
        if username.contains('\0') {
            return Err(AuthenticationError::EmptyUsername);
        }

        match &self.dn_template {
            Some(template) => self.authenticate_by_template(template, username, password).await,
            None => self.authenticate_by_search(username, password).await,
        }
    }

    async fn authenticate_by_search(
        &self,
        username: &str,
        password: &str,
    ) -> std::result::Result<User, AuthenticationError> {
        let entry = self.find(username).await?;

        // A second connection, not the service one. A bind replaces the
        // connection's authorization identity, so binding as the user here
        // would silently demote the service connection — and a *failed* bind
        // leaves a connection anonymous rather than closed, which is worse: the
        // next search on it would run with no rights and look like an empty
        // directory.
        let mut connection = self.connect().await?;
        let result = connection
            .bind(&entry.dn, password)
            .await
            .map_err(AuthenticationError::Unreachable)?;
        let _ = connection.unbind().await;

        classify(result)?;
        Ok(User::from(entry))
    }

    async fn authenticate_by_template(
        &self,
        template: &str,
        username: &str,
        password: &str,
    ) -> std::result::Result<User, AuthenticationError> {
        // Escaped as a DN value: a comma ends an RDN, so an unescaped one in a
        // username would move the bind to an entry the template never named.
        let dn = template.replace("{username}", &crate::protocol::escape_dn_value(username));

        let mut connection = self.connect().await?;
        let result =
            connection.bind(&dn, password).await.map_err(AuthenticationError::Unreachable)?;
        let _ = connection.unbind().await;

        classify(result)?;
        Ok(User { dn, attributes: Vec::new() })
    }

    /// Find a user's entry without checking their password.
    ///
    /// The search half of [`Directory::authenticate`], separated because
    /// "does this account exist" is a question worth asking on its own — during
    /// provisioning, or when a session outlives the account behind it.
    pub async fn find(
        &self,
        username: &str,
    ) -> std::result::Result<SearchEntry, AuthenticationError> {
        if username.is_empty() || username.contains('\0') {
            return Err(AuthenticationError::EmptyUsername);
        }

        let mut connection = self.connect().await?;

        // Bind as the service account first: an anonymous client usually cannot
        // read `uid` at all, and a directory that answers an anonymous search
        // with an empty result is indistinguishable from one where the user
        // does not exist.
        match &self.service {
            ServiceIdentity::Anonymous => connection
                .bind_anonymous()
                .await
                .map_err(AuthenticationError::ServiceAccount)?
                .into_result("anonymous bind")
                .map_err(AuthenticationError::ServiceAccount)?,
            ServiceIdentity::Simple { dn, password } => connection
                .bind(dn, password)
                .await
                .map_err(AuthenticationError::ServiceAccount)?
                .into_result("service account bind")
                .map_err(AuthenticationError::ServiceAccount)?,
        };

        // The username goes in as a *value*. A `*` here is one byte, 0x2a,
        // compared literally — not a wildcard, because BER octet strings have
        // no metacharacters. This is the whole reason `Filter` is a type.
        let matches_username = Filter::equals(&self.username_attribute, username);
        let filter = match &self.extra_filter {
            Some(extra) => Filter::and([extra.clone(), matches_username]),
            None => matches_username,
        };

        let request = SearchRequest::new(&self.base_dn, filter)
            .scope(self.scope)
            // Two, not one: one entry is an answer, two is an ambiguity worth
            // refusing, and asking for a third would be paying for a number
            // nobody acts on.
            .size_limit(2)
            .attributes(self.attributes.clone());

        let outcome =
            connection.search(&request).await.map_err(AuthenticationError::Unreachable)?;
        let _ = connection.unbind().await;

        // sizeLimitExceeded is a success as far as this is concerned: it means
        // the directory stopped early, which only happens when there was more
        // than one match, which is the ambiguity below.
        if !outcome.result.is_success() && outcome.result.code != ResultCode::SizeLimitExceeded {
            return Err(AuthenticationError::Directory(outcome.result));
        }

        match outcome.entries.len() {
            0 => Err(AuthenticationError::NoSuchUser),
            1 => Ok(outcome.entries.into_iter().next().expect("length checked")),
            // Two accounts with the same username is a directory that has been
            // misconfigured, and picking the first one means picking somebody
            // arbitrary to log in as.
            matched => Err(AuthenticationError::Ambiguous { matched }),
        }
    }

    /// Run an arbitrary search as the service account.
    ///
    /// The escape hatch, for group membership and anything else this package
    /// does not model. Still a typed [`Filter`], so it is still not a template.
    pub async fn search(&self, filter: Filter, attributes: &[&str]) -> Result<Vec<SearchEntry>> {
        let mut connection = LdapConnection::connect(&self.config).await?;

        match &self.service {
            ServiceIdentity::Anonymous => {
                connection.bind_anonymous().await?.into_result("anonymous bind")?
            }
            ServiceIdentity::Simple { dn, password } => {
                connection.bind(dn, password).await?.into_result("service account bind")?
            }
        };

        let request =
            SearchRequest::new(&self.base_dn, filter).scope(self.scope).attributes(attributes.iter().copied());
        let outcome = connection.search(&request).await?;
        let _ = connection.unbind().await;

        outcome.result.into_result("search")?;
        Ok(outcome.entries)
    }

    async fn connect(&self) -> std::result::Result<LdapConnection, AuthenticationError> {
        LdapConnection::connect(&self.config).await.map_err(AuthenticationError::Unreachable)
    }
}

/// Turn a bind's result code into the most specific failure it supports.
fn classify(result: LdapResult) -> std::result::Result<(), AuthenticationError> {
    if result.is_success() {
        return Ok(());
    }

    match result.code {
        ResultCode::InvalidCredentials => match AccountProblem::from_diagnostic(&result.diagnostic)
        {
            // Active Directory answers everything with 49 and hides the real
            // reason in the diagnostic. Reading it is the difference between
            // "wrong password" and "this account was disabled in March".
            Some(AccountProblem::NoSuchAccount) => Err(AuthenticationError::NoSuchUser),
            Some(AccountProblem::WrongPassword) | None => {
                Err(AuthenticationError::InvalidPassword)
            }
            Some(reason) => {
                Err(AuthenticationError::AccountUnusable { reason, diagnostic: result.diagnostic })
            }
        },
        // "I will not do that", which for a bind means a policy said no: a
        // locked account, or a password that must be changed first.
        ResultCode::UnwillingToPerform | ResultCode::InappropriateAuthentication => {
            Err(AuthenticationError::AccountUnusable {
                reason: AccountProblem::from_diagnostic(&result.diagnostic)
                    .unwrap_or(AccountProblem::Unspecified),
                diagnostic: result.diagnostic,
            })
        }
        // The entry the search just found is gone, or was never bindable.
        ResultCode::NoSuchObject => Err(AuthenticationError::NoSuchUser),
        _ => Err(AuthenticationError::Directory(result)),
    }
}

/// Why a directory would not let an account in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountProblem {
    /// Active Directory's `data 525`: the account does not exist. Surfaced as
    /// [`AuthenticationError::NoSuchUser`] rather than kept here.
    NoSuchAccount,
    /// `data 52e`: the password is wrong, and only the password.
    WrongPassword,
    /// `data 530`: not permitted to log on at this time of day.
    TimeRestricted,
    /// `data 531`: not permitted to log on from this workstation.
    WorkstationRestricted,
    /// `data 532`: the password has expired.
    PasswordExpired,
    /// `data 533`: the account is disabled.
    Disabled,
    /// `data 701`: the account itself has expired.
    Expired,
    /// `data 773`: the user must change their password before logging in.
    MustChangePassword,
    /// `data 775`: the account is locked out.
    Locked,
    /// The directory refused without saying which of these it was.
    Unspecified,
}

impl AccountProblem {
    /// Read Active Directory's hexadecimal sub-code out of a diagnostic.
    ///
    /// AD answers every bind failure with `invalidCredentials` and puts the
    /// real reason in a message shaped like
    /// `80090308: LdapErr: DSID-0C0903A9, comment: AcceptSecurityContext error,
    /// data 533, v2580`. It is undocumented in any RFC and completely stable in
    /// practice, and without it a help desk cannot tell a forgotten password
    /// from a disabled account.
    ///
    /// Anything that is not Active Directory returns `None` here, which is the
    /// right answer: OpenLDAP means exactly what its result code says.
    pub fn from_diagnostic(diagnostic: &str) -> Option<AccountProblem> {
        let after = diagnostic.split("data ").nth(1)?;
        let code: String =
            after.chars().take_while(|character| character.is_ascii_hexdigit()).collect();

        match code.to_ascii_lowercase().as_str() {
            "525" => Some(AccountProblem::NoSuchAccount),
            "52e" => Some(AccountProblem::WrongPassword),
            "530" => Some(AccountProblem::TimeRestricted),
            "531" => Some(AccountProblem::WorkstationRestricted),
            "532" => Some(AccountProblem::PasswordExpired),
            "533" => Some(AccountProblem::Disabled),
            "701" => Some(AccountProblem::Expired),
            "773" => Some(AccountProblem::MustChangePassword),
            "775" => Some(AccountProblem::Locked),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AccountProblem::NoSuchAccount => "the account does not exist",
            AccountProblem::WrongPassword => "the password is wrong",
            AccountProblem::TimeRestricted => "the account may not log in at this time of day",
            AccountProblem::WorkstationRestricted => {
                "the account may not log in from this workstation"
            }
            AccountProblem::PasswordExpired => "the password has expired",
            AccountProblem::Disabled => "the account is disabled",
            AccountProblem::Expired => "the account has expired",
            AccountProblem::MustChangePassword => "the password must be changed before logging in",
            AccountProblem::Locked => "the account is locked out",
            AccountProblem::Unspecified => "the directory refused without saying why",
        }
    }
}

impl std::fmt::Display for AccountProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an authentication attempt did not succeed.
///
/// Every variant is distinguishable so that a log can say what happened. Not
/// every variant may be *shown* to the person logging in — see
/// [`AuthenticationError::is_credential_failure`].
#[derive(Debug)]
pub enum AuthenticationError {
    /// The password field was empty.
    ///
    /// Its own variant rather than an `InvalidPassword`, because this one never
    /// touched the network: LDAP would have answered it with `success`, and
    /// this is the client refusing to ask.
    EmptyPassword,
    /// The username field was empty, or held a NUL.
    EmptyUsername,
    /// The search found nobody.
    NoSuchUser,
    /// The entry exists and the password is wrong.
    InvalidPassword,
    /// More than one entry has that username. Refused rather than resolved:
    /// picking one means picking somebody arbitrary to log in as.
    Ambiguous { matched: usize },
    /// The password may well be right, but a policy says no.
    AccountUnusable { reason: AccountProblem, diagnostic: String },
    /// The directory could not be reached, or the connection failed part way.
    Unreachable(Error),
    /// The client's own service account could not bind. Not the user's problem,
    /// and worth paging somebody about.
    ServiceAccount(Error),
    /// The directory refused for a reason this package does not model.
    Directory(LdapResult),
}

impl AuthenticationError {
    /// Whether this is "the credentials were wrong" in any of its forms.
    ///
    /// Show one identical message for every case where this is true. A form
    /// that distinguishes "no such user" from "wrong password" tells an
    /// attacker which usernames are real, one guess at a time — and it does it
    /// for free, at any rate they like.
    ///
    /// Everything this returns false for is an operational problem, and saying
    /// "we could not reach the directory" for those is both honest and safe.
    pub fn is_credential_failure(&self) -> bool {
        matches!(
            self,
            AuthenticationError::EmptyPassword
                | AuthenticationError::EmptyUsername
                | AuthenticationError::NoSuchUser
                | AuthenticationError::InvalidPassword
        )
    }

    /// Whether the directory itself is at fault rather than the credentials.
    pub fn is_directory_failure(&self) -> bool {
        matches!(
            self,
            AuthenticationError::Unreachable(_)
                | AuthenticationError::ServiceAccount(_)
                | AuthenticationError::Directory(_)
                | AuthenticationError::Ambiguous { .. }
        )
    }
}

impl std::fmt::Display for AuthenticationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthenticationError::EmptyPassword => f.write_str(
                "the password was empty; LDAP would have treated that as an anonymous bind and \
                 answered `success`, so it was refused here instead",
            ),
            AuthenticationError::EmptyUsername => f.write_str("the username was empty"),
            AuthenticationError::NoSuchUser => {
                f.write_str("the directory has no entry with that username")
            }
            AuthenticationError::InvalidPassword => {
                f.write_str("the directory rejected that password")
            }
            AuthenticationError::Ambiguous { matched } => write!(
                f,
                "{matched} entries share that username, so there is no one account to log in as"
            ),
            AuthenticationError::AccountUnusable { reason, diagnostic } => {
                write!(f, "the directory would not let that account log in: {reason}")?;
                if !diagnostic.is_empty() {
                    write!(f, " ({diagnostic})")?;
                }
                Ok(())
            }
            AuthenticationError::Unreachable(error) => {
                write!(f, "the directory could not be reached: {error}")
            }
            AuthenticationError::ServiceAccount(error) => {
                write!(f, "this application could not bind as its own service account: {error}")
            }
            AuthenticationError::Directory(result) => {
                write!(f, "the directory refused the bind: {}", result.code)?;
                if !result.diagnostic.is_empty() {
                    write!(f, " — {}", result.diagnostic)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for AuthenticationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AuthenticationError::Unreachable(error)
            | AuthenticationError::ServiceAccount(error) => Some(error),
            _ => None,
        }
    }
}

impl From<AuthenticationError> for Error {
    fn from(error: AuthenticationError) -> Error {
        Error::msg(error.to_string())
    }
}

/// A user the directory recognised, with whatever attributes were asked for.
#[derive(Debug, Clone)]
pub struct User {
    /// The entry's distinguished name — the only identifier a directory
    /// guarantees is unique and stable enough to key a session on.
    pub dn: String,
    pub attributes: Vec<Attribute>,
}

impl User {
    pub fn dn(&self) -> &str {
        &self.dn
    }

    /// The first value of an attribute, as text. Case-insensitive, because
    /// attribute descriptions are.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
            .and_then(Attribute::first)
    }

    /// Every value of an attribute that is valid text.
    pub fn values(&self, name: &str) -> Vec<&str> {
        self.attributes
            .iter()
            .find(|attribute| attribute.name.eq_ignore_ascii_case(name))
            .map(|attribute| {
                attribute
                    .values
                    .iter()
                    .filter_map(|value| std::str::from_utf8(value).ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl From<SearchEntry> for User {
    fn from(entry: SearchEntry) -> User {
        User { dn: entry.dn, attributes: entry.attributes }
    }
}

/// The DN is what a session remembers.
///
/// Not the username: a person can be renamed, and two directories can disagree
/// about which attribute a username lives in. A DN is the directory's own
/// answer to "which entry is this".
impl Authenticatable for User {
    fn auth_identifier(&self) -> String {
        self.dn.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_empty_password_is_refused_before_it_reaches_the_wire() {
        // The bug this prevents, stated plainly: RFC 4511 §4.2 says a simple
        // bind with a zero-length password is an *anonymous* bind, and a
        // directory answers it with `success`. A login form that passed a blank
        // password field straight through would therefore report a successful
        // login for any username that exists. It has been a real CVE in real
        // products more than once.
        //
        // This directory points at a port with nothing behind it. If the check
        // were anywhere but in front of the I/O, the error would be a
        // connection failure — so the assertion is as much about *which* error
        // comes back as about the fact that one does.
        let directory = Directory::new("ldaps://127.0.0.1:1")
            .unwrap()
            .base_dn("dc=example,dc=test")
            .service_account("cn=svc,dc=example,dc=test", "secret")
            .connect_timeout(Duration::from_millis(200));

        let error = directory.authenticate("alice", "").await.unwrap_err();

        assert!(
            matches!(error, AuthenticationError::EmptyPassword),
            "an empty password must be its own answer, not a connection failure: {error}"
        );
        assert!(error.is_credential_failure());
        assert!(error.to_string().contains("anonymous"), "and say why: {error}");

        // For contrast: with a password, the same call gets as far as the
        // socket and fails there. That is the proof the guard above is the
        // thing that stopped it, and not the network.
        let error = directory.authenticate("alice", "hunter2").await.unwrap_err();
        assert!(
            matches!(error, AuthenticationError::Unreachable(_)),
            "expected to reach the network, got {error}"
        );

        // A password of a single space is a password.
        let error = directory.authenticate("alice", " ").await.unwrap_err();
        assert!(matches!(error, AuthenticationError::Unreachable(_)), "got {error}");
    }

    #[tokio::test]
    async fn an_empty_username_is_refused_too() {
        let directory = Directory::new("ldaps://127.0.0.1:1")
            .unwrap()
            .connect_timeout(Duration::from_millis(200));

        assert!(matches!(
            directory.authenticate("", "hunter2").await.unwrap_err(),
            AuthenticationError::EmptyUsername
        ));
        // A NUL is not something a person types; it is something a person
        // sends, usually to truncate a string somewhere downstream.
        assert!(matches!(
            directory.authenticate("alice\0", "hunter2").await.unwrap_err(),
            AuthenticationError::EmptyUsername
        ));
    }

    #[test]
    fn a_username_becomes_a_value_and_never_a_filter() {
        // The filter the search would build for a hostile username. It is one
        // equality assertion with an eighteen-byte value, not two clauses — the
        // parentheses never had any syntax to escape into.
        let filter = Filter::and([
            Filter::equals("objectClass", "inetOrgPerson"),
            Filter::equals("uid", "*)(uid=admin"),
        ]);

        let mut encoder = crate::ber::Encoder::new();
        filter.encode(&mut encoder);
        let bytes = encoder.into_bytes();

        // Two equality assertions inside one `and`, and the injected text sits
        // whole inside an octet string of its own length.
        assert_eq!(bytes[0], 0xa0);
        assert_eq!(bytes.iter().filter(|&&byte| byte == 0xa3).count(), 2);
        assert!(
            bytes.windows(12).any(|window| window == b"*)(uid=admin"),
            "the value is carried verbatim, as data"
        );

        // And the string form, which is what a template would have produced,
        // escapes all three metacharacters.
        assert_eq!(
            filter.to_string(),
            "(&(objectClass=inetOrgPerson)(uid=\\2a\\29\\28uid=admin))"
        );
    }

    #[test]
    fn active_directory_s_sub_codes_are_read_out_of_the_diagnostic() {
        // The real shape of an AD diagnostic, verbatim.
        let disabled = "80090308: LdapErr: DSID-0C0903A9, comment: AcceptSecurityContext error, \
                        data 533, v2580";
        assert_eq!(AccountProblem::from_diagnostic(disabled), Some(AccountProblem::Disabled));

        for (code, expected) in [
            ("525", AccountProblem::NoSuchAccount),
            ("52e", AccountProblem::WrongPassword),
            ("530", AccountProblem::TimeRestricted),
            ("531", AccountProblem::WorkstationRestricted),
            ("532", AccountProblem::PasswordExpired),
            ("533", AccountProblem::Disabled),
            ("701", AccountProblem::Expired),
            ("773", AccountProblem::MustChangePassword),
            ("775", AccountProblem::Locked),
        ] {
            let diagnostic = format!("80090308: LdapErr: ..., data {code}, v2580");
            assert_eq!(AccountProblem::from_diagnostic(&diagnostic), Some(expected), "{code}");
            // AD is inconsistent about the case of the hex digit.
            let upper = format!("80090308: LdapErr: ..., data {}, v2580", code.to_uppercase());
            assert_eq!(AccountProblem::from_diagnostic(&upper), Some(expected), "{code}");
        }

        // OpenLDAP means what its result code says and has no sub-code.
        assert_eq!(AccountProblem::from_diagnostic(""), None);
        assert_eq!(AccountProblem::from_diagnostic("Invalid credentials"), None);
        assert_eq!(AccountProblem::from_diagnostic("..., data 999, ..."), None);
    }

    #[test]
    fn a_disabled_account_is_not_reported_as_a_wrong_password() {
        // Both come back as result code 49. Telling them apart is the point:
        // one is the user's mistake, the other is a help desk ticket.
        let wrong = LdapResult {
            code: ResultCode::InvalidCredentials,
            matched_dn: String::new(),
            diagnostic: "80090308: LdapErr: ..., data 52e, v2580".into(),
            referrals: Vec::new(),
        };
        assert!(matches!(classify(wrong).unwrap_err(), AuthenticationError::InvalidPassword));

        let disabled = LdapResult {
            code: ResultCode::InvalidCredentials,
            matched_dn: String::new(),
            diagnostic: "80090308: LdapErr: ..., data 533, v2580".into(),
            referrals: Vec::new(),
        };
        let error = classify(disabled).unwrap_err();
        assert!(matches!(
            error,
            AuthenticationError::AccountUnusable { reason: AccountProblem::Disabled, .. }
        ));
        assert!(error.to_string().contains("disabled"), "got {error}");

        // A disabled account is not a credential failure: showing "wrong
        // password" for it sends the user round in circles.
        assert!(!error.is_credential_failure());

        // OpenLDAP's plain 49, with nothing to read: the password was wrong.
        let plain = LdapResult {
            code: ResultCode::InvalidCredentials,
            matched_dn: String::new(),
            diagnostic: String::new(),
            referrals: Vec::new(),
        };
        assert!(matches!(classify(plain).unwrap_err(), AuthenticationError::InvalidPassword));

        // And success is success.
        assert!(
            classify(LdapResult {
                code: ResultCode::Success,
                matched_dn: String::new(),
                diagnostic: String::new(),
                referrals: Vec::new(),
            })
            .is_ok()
        );
    }

    #[test]
    fn credential_failures_are_grouped_so_a_form_can_say_one_thing() {
        for error in [
            AuthenticationError::EmptyPassword,
            AuthenticationError::EmptyUsername,
            AuthenticationError::NoSuchUser,
            AuthenticationError::InvalidPassword,
        ] {
            assert!(error.is_credential_failure(), "{error}");
            assert!(!error.is_directory_failure(), "{error}");
        }

        for error in [
            AuthenticationError::Unreachable(Error::msg("connection refused")),
            AuthenticationError::ServiceAccount(Error::msg("bad password")),
            AuthenticationError::Ambiguous { matched: 2 },
        ] {
            assert!(!error.is_credential_failure(), "{error}");
            assert!(error.is_directory_failure(), "{error}");
        }
    }

    #[test]
    fn a_user_is_keyed_on_its_dn_and_reads_attributes_case_insensitively() {
        let user = User::from(SearchEntry {
            dn: "uid=alice,ou=people,dc=example,dc=test".into(),
            attributes: vec![
                Attribute { name: "cn".into(), values: vec![b"Alice Liddell".to_vec()] },
                Attribute {
                    name: "memberOf".into(),
                    values: vec![b"cn=staff".to_vec(), b"cn=admins".to_vec()],
                },
            ],
        });

        assert_eq!(user.auth_identifier(), "uid=alice,ou=people,dc=example,dc=test");
        assert_eq!(user.value("CN"), Some("Alice Liddell"));
        assert_eq!(user.values("memberof"), vec!["cn=staff", "cn=admins"]);
        assert_eq!(user.value("mail"), None);
        assert!(user.values("mail").is_empty());
    }

    #[test]
    fn a_dn_template_escapes_the_username_into_the_dn() {
        // Not an authentication test — the substitution itself, because getting
        // it wrong moves the bind to an entry the template never described.
        let template = "uid={username},ou=people,dc=example,dc=test";
        let hostile = crate::protocol::escape_dn_value("alice,ou=admins");

        assert_eq!(
            template.replace("{username}", &hostile),
            "uid=alice\\,ou\\=admins,ou=people,dc=example,dc=test",
            "the comma stays inside the first RDN instead of starting a new one"
        );
    }
}
