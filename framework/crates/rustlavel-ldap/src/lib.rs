//! rustlavel-ldap: authenticate a person against a directory.
//!
//! LDAP is how most organisations answer "who is this and what is their
//! password", and an application that needs that answer should not need to
//! learn ASN.1 to get it. So the package has two faces: [`directory`], which is
//! one call, and [`protocol`] and [`ber`] underneath it for anything that call
//! does not cover.
//!
//! ```ignore
//! use rustlavel_ldap::prelude::*;
//!
//! let directory = Directory::new("ldaps://dc.example.test")?
//!     .service_account("cn=svc,dc=example,dc=test", &service_password)
//!     .base_dn("ou=people,dc=example,dc=test")
//!     .username_attribute("uid")
//!     .filter(Filter::equals("objectClass", "inetOrgPerson"))
//!     .attributes(["cn", "mail"]);
//!
//! let user = directory.authenticate(&username, &password).await?;
//! println!("{} is {}", user.dn(), user.value("cn").unwrap_or("unnamed"));
//! ```
//!
//! # The protocol is written here
//!
//! LDAP v3 (RFC 4511) is ASN.1 BER over TCP. BER is a serialisation format
//! rather than cryptography, so rule one applies and it is written in [`ber`]:
//! a small, strict subset that refuses everything RFC 4511 says a directory may
//! not send. TLS is the exception, and comes from rustls with the `aws_lc_rs`
//! provider and `prefer-post-quantum`, like every other TLS in this framework.
//!
//! # Three things this package is firm about
//!
//! **A simple bind sends the password in the clear.** That is what a simple
//! bind *is* — RFC 4511 §4.2 puts the password in an octet string with nothing
//! around it. So one is refused over an unencrypted connection unless the
//! caller has said, in as many words, that it is acceptable
//! ([`LdapConfig::allow_plaintext_password`]). This is the most common LDAP
//! deployment mistake there is, and it is invisible from the outside because
//! everything works.
//!
//! **An empty password must not authenticate.** LDAP treats a simple bind with
//! a zero-length password as an *anonymous* bind, which succeeds — so a login
//! form with a blank password field would report a successful login for any
//! username that exists. This has been a real vulnerability in real products
//! more than once. It is refused in [`Operation::simple_bind`], where the bytes
//! are built, and again in [`Directory::authenticate`] before any I/O happens.
//!
//! **A username is a value, not text.** The classic LDAP injection is a filter
//! template with a name pasted into it: a username of `*` turns a lookup into a
//! wildcard that matches every account. [`Filter`] is a type, and
//! [`Filter::encode`] writes each value as a length-delimited BER octet string
//! where `*` and `)` have no syntax at all. [`escape_filter_value`] exists for
//! the cases that genuinely need the RFC 4515 string form.

pub mod ber;
pub mod connection;
pub mod directory;
pub mod protocol;

pub use connection::{Encryption, LdapConfig, LdapConnection};
pub use directory::{AccountProblem, AuthenticationError, Directory, User};
pub use protocol::{
    Attribute, Control, DerefAliases, Filter, LdapMessage, LdapResult, Operation, ProtocolOp,
    ResultCode, Scope, SearchEntry, SearchOutcome, SearchRequest, escape_dn_value,
    escape_filter_value,
};

pub use rustlavel_core::{Error, Result};

/// What an application importing this package normally wants.
pub mod prelude {
    pub use crate::connection::{Encryption, LdapConfig, LdapConnection};
    pub use crate::directory::{AccountProblem, AuthenticationError, Directory, User};
    pub use crate::protocol::{
        Filter, ResultCode, Scope, SearchEntry, SearchRequest, escape_dn_value,
        escape_filter_value,
    };
}
