//! LDAP v3 messages, as RFC 4511 defines them.
//!
//! Every exchange is an `LDAPMessage`: a message id, one protocol operation,
//! and optional controls. The id is how a client matches an answer to a
//! question — LDAP allows several operations to be in flight on one connection,
//! so nothing about a response says which request it belongs to except that
//! number.
//!
//! The operations here are the ones an application actually needs to
//! authenticate somebody: bind, search, unbind, and the StartTLS extended
//! request. Modify, add and delete are deliberately absent — this package
//! exists to answer "is this the right password", and a directory is somebody
//! else's system of record.
//!
//! ## Tags
//!
//! RFC 4511's ASN.1 module is `IMPLICIT TAGS`, so a context tag *replaces* the
//! underlying universal one rather than wrapping it: `and [0] SET OF Filter`
//! goes on the wire as `0xa0`, not as `0xa0` around a `0x31`. The one exception
//! is `not [2] Filter`, because a tag on a CHOICE is always explicit — so a
//! `not` filter's contents are a complete inner filter element. Both cases are
//! encoded and tested below, because getting this backwards produces bytes a
//! directory rejects with a protocolError and no useful explanation.

use crate::ber::{self, Decoder, Encoder};
use rustlavel_core::{Error, Result};

/// The protocol version this client speaks. LDAP v2 is not implemented, and
/// should not be: it has no StartTLS and no UTF-8 guarantee.
pub const VERSION: i64 = 3;

/// The OID of the StartTLS extended operation (RFC 4511 §4.14.1).
pub const START_TLS_OID: &str = "1.3.6.1.4.1.1466.20037";

/// `[APPLICATION n]` tags for the protocol operations, from RFC 4511 §4.
pub mod tags {
    use crate::ber::application;

    pub const BIND_REQUEST: u8 = application(0, true);
    pub const BIND_RESPONSE: u8 = application(1, true);
    /// `UnbindRequest ::= [APPLICATION 2] NULL` — primitive, and always empty.
    pub const UNBIND_REQUEST: u8 = application(2, false);
    pub const SEARCH_REQUEST: u8 = application(3, true);
    pub const SEARCH_RESULT_ENTRY: u8 = application(4, true);
    pub const SEARCH_RESULT_DONE: u8 = application(5, true);
    pub const SEARCH_RESULT_REFERENCE: u8 = application(19, true);
    pub const EXTENDED_REQUEST: u8 = application(23, true);
    pub const EXTENDED_RESPONSE: u8 = application(24, true);
    pub const INTERMEDIATE_RESPONSE: u8 = application(25, true);
}

// ---------------------------------------------------------------------------
// Result codes
// ---------------------------------------------------------------------------

/// Every result code RFC 4511 names, plus whatever else a directory sends.
///
/// This is an enum rather than a bare integer because the difference between
/// 32 and 49 is the difference between "no such user" and "wrong password", and
/// a caller that has to remember which number is which will eventually get it
/// wrong in the direction that logs somebody in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultCode {
    Success,
    OperationsError,
    ProtocolError,
    TimeLimitExceeded,
    SizeLimitExceeded,
    CompareFalse,
    CompareTrue,
    AuthMethodNotSupported,
    StrongerAuthRequired,
    Referral,
    AdminLimitExceeded,
    UnavailableCriticalExtension,
    ConfidentialityRequired,
    SaslBindInProgress,
    NoSuchAttribute,
    UndefinedAttributeType,
    InappropriateMatching,
    ConstraintViolation,
    AttributeOrValueExists,
    InvalidAttributeSyntax,
    NoSuchObject,
    AliasProblem,
    InvalidDnSyntax,
    AliasDereferencingProblem,
    InappropriateAuthentication,
    InvalidCredentials,
    InsufficientAccessRights,
    Busy,
    Unavailable,
    UnwillingToPerform,
    LoopDetect,
    NamingViolation,
    ObjectClassViolation,
    NotAllowedOnNonLeaf,
    NotAllowedOnRdn,
    EntryAlreadyExists,
    ObjectClassModsProhibited,
    AffectsMultipleDsas,
    Other,
    /// A code this client does not know. Directories add their own; refusing to
    /// carry the number would lose the only clue in the message.
    Unknown(i64),
}

impl ResultCode {
    pub fn from_i64(code: i64) -> ResultCode {
        use ResultCode::*;
        match code {
            0 => Success,
            1 => OperationsError,
            2 => ProtocolError,
            3 => TimeLimitExceeded,
            4 => SizeLimitExceeded,
            5 => CompareFalse,
            6 => CompareTrue,
            7 => AuthMethodNotSupported,
            8 => StrongerAuthRequired,
            10 => Referral,
            11 => AdminLimitExceeded,
            12 => UnavailableCriticalExtension,
            13 => ConfidentialityRequired,
            14 => SaslBindInProgress,
            16 => NoSuchAttribute,
            17 => UndefinedAttributeType,
            18 => InappropriateMatching,
            19 => ConstraintViolation,
            20 => AttributeOrValueExists,
            21 => InvalidAttributeSyntax,
            32 => NoSuchObject,
            33 => AliasProblem,
            34 => InvalidDnSyntax,
            36 => AliasDereferencingProblem,
            48 => InappropriateAuthentication,
            49 => InvalidCredentials,
            50 => InsufficientAccessRights,
            51 => Busy,
            52 => Unavailable,
            53 => UnwillingToPerform,
            54 => LoopDetect,
            64 => NamingViolation,
            65 => ObjectClassViolation,
            66 => NotAllowedOnNonLeaf,
            67 => NotAllowedOnRdn,
            68 => EntryAlreadyExists,
            69 => ObjectClassModsProhibited,
            71 => AffectsMultipleDsas,
            80 => Other,
            other => Unknown(other),
        }
    }

    pub fn as_i64(self) -> i64 {
        use ResultCode::*;
        match self {
            Success => 0,
            OperationsError => 1,
            ProtocolError => 2,
            TimeLimitExceeded => 3,
            SizeLimitExceeded => 4,
            CompareFalse => 5,
            CompareTrue => 6,
            AuthMethodNotSupported => 7,
            StrongerAuthRequired => 8,
            Referral => 10,
            AdminLimitExceeded => 11,
            UnavailableCriticalExtension => 12,
            ConfidentialityRequired => 13,
            SaslBindInProgress => 14,
            NoSuchAttribute => 16,
            UndefinedAttributeType => 17,
            InappropriateMatching => 18,
            ConstraintViolation => 19,
            AttributeOrValueExists => 20,
            InvalidAttributeSyntax => 21,
            NoSuchObject => 32,
            AliasProblem => 33,
            InvalidDnSyntax => 34,
            AliasDereferencingProblem => 36,
            InappropriateAuthentication => 48,
            InvalidCredentials => 49,
            InsufficientAccessRights => 50,
            Busy => 51,
            Unavailable => 52,
            UnwillingToPerform => 53,
            LoopDetect => 54,
            NamingViolation => 64,
            ObjectClassViolation => 65,
            NotAllowedOnNonLeaf => 66,
            NotAllowedOnRdn => 67,
            EntryAlreadyExists => 68,
            ObjectClassModsProhibited => 69,
            AffectsMultipleDsas => 71,
            Other => 80,
            Unknown(code) => code,
        }
    }

    /// The name RFC 4511 gives the code, in the spelling the RFC uses.
    pub fn name(self) -> &'static str {
        use ResultCode::*;
        match self {
            Success => "success",
            OperationsError => "operationsError",
            ProtocolError => "protocolError",
            TimeLimitExceeded => "timeLimitExceeded",
            SizeLimitExceeded => "sizeLimitExceeded",
            CompareFalse => "compareFalse",
            CompareTrue => "compareTrue",
            AuthMethodNotSupported => "authMethodNotSupported",
            StrongerAuthRequired => "strongerAuthRequired",
            Referral => "referral",
            AdminLimitExceeded => "adminLimitExceeded",
            UnavailableCriticalExtension => "unavailableCriticalExtension",
            ConfidentialityRequired => "confidentialityRequired",
            SaslBindInProgress => "saslBindInProgress",
            NoSuchAttribute => "noSuchAttribute",
            UndefinedAttributeType => "undefinedAttributeType",
            InappropriateMatching => "inappropriateMatching",
            ConstraintViolation => "constraintViolation",
            AttributeOrValueExists => "attributeOrValueExists",
            InvalidAttributeSyntax => "invalidAttributeSyntax",
            NoSuchObject => "noSuchObject",
            AliasProblem => "aliasProblem",
            InvalidDnSyntax => "invalidDNSyntax",
            AliasDereferencingProblem => "aliasDereferencingProblem",
            InappropriateAuthentication => "inappropriateAuthentication",
            InvalidCredentials => "invalidCredentials",
            InsufficientAccessRights => "insufficientAccessRights",
            Busy => "busy",
            Unavailable => "unavailable",
            UnwillingToPerform => "unwillingToPerform",
            LoopDetect => "loopDetect",
            NamingViolation => "namingViolation",
            ObjectClassViolation => "objectClassViolation",
            NotAllowedOnNonLeaf => "notAllowedOnNonLeaf",
            NotAllowedOnRdn => "notAllowedOnRDN",
            EntryAlreadyExists => "entryAlreadyExists",
            ObjectClassModsProhibited => "objectClassModsProhibited",
            AffectsMultipleDsas => "affectsMultipleDSAs",
            Other => "other",
            Unknown(_) => "an unrecognised result code",
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, ResultCode::Success)
    }
}

impl std::fmt::Display for ResultCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.name(), self.as_i64())
    }
}

/// `LDAPResult` — the shape every operation's answer ends with.
#[derive(Debug, Clone)]
pub struct LdapResult {
    pub code: ResultCode,
    /// How much of the requested DN the directory did find. Empty most of the
    /// time; useful when a base DN has a typo in it.
    pub matched_dn: String,
    /// The directory's own words. Active Directory hides its real reason in
    /// here as a hex sub-code, which [`crate::directory`] reads.
    pub diagnostic: String,
    pub referrals: Vec<String>,
}

impl LdapResult {
    pub fn is_success(&self) -> bool {
        self.code.is_success()
    }

    /// Turn anything but success into an error that says what failed and why.
    pub fn into_result(self, operation: &str) -> Result<LdapResult> {
        if self.is_success() {
            return Ok(self);
        }

        let mut message = format!("the directory refused the {operation}: {}", self.code);
        if !self.diagnostic.is_empty() {
            message.push_str(&format!(" — {}", self.diagnostic));
        }
        if !self.matched_dn.is_empty() {
            message.push_str(&format!(" (it did find `{}`)", self.matched_dn));
        }
        Err(Error::msg(message))
    }

    /// `COMPONENTS OF LDAPResult`: four fields inlined into the operation's own
    /// SEQUENCE, not a nested one. Reading them as a nested SEQUENCE is the
    /// single easiest mistake to make here.
    fn decode(decoder: &mut Decoder<'_>) -> Result<LdapResult> {
        let code = ResultCode::from_i64(decoder.enumerated()?);
        let matched_dn = decoder.string()?;
        let diagnostic = decoder.string()?;

        let mut referrals = Vec::new();
        if decoder.has_tag(ber::context(3, true)) {
            let mut list = decoder.nested(ber::context(3, true))?;
            while !list.is_empty() {
                referrals.push(list.string()?);
            }
        }

        Ok(LdapResult { code, matched_dn, diagnostic, referrals })
    }
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// A request or response control (RFC 4511 §4.1.11).
///
/// The value stays as raw octets: every control defines its own encoding, and
/// this package has no business guessing at one it was not told about.
#[derive(Debug, Clone)]
pub struct Control {
    pub oid: String,
    /// When true, a directory that does not implement the control must refuse
    /// the whole operation rather than quietly ignoring it.
    pub critical: bool,
    pub value: Option<Vec<u8>>,
}

impl Control {
    pub fn new(oid: impl Into<String>) -> Control {
        Control { oid: oid.into(), critical: false, value: None }
    }

    pub fn critical(mut self, critical: bool) -> Control {
        self.critical = critical;
        self
    }

    pub fn value(mut self, value: impl Into<Vec<u8>>) -> Control {
        self.value = Some(value.into());
        self
    }

    fn encode(&self, encoder: &mut Encoder) {
        encoder.sequence(|body| {
            body.string(&self.oid);
            // DEFAULT FALSE: omitted when false, because a directory comparing
            // encodings expects the default to be absent.
            if self.critical {
                body.boolean(true);
            }
            if let Some(value) = &self.value {
                body.octet_string(value);
            }
        });
    }

    fn decode(decoder: &mut Decoder<'_>) -> Result<Control> {
        let mut fields = decoder.sequence()?;
        let oid = fields.string()?;
        let mut critical = false;
        let mut value = None;

        if fields.has_tag(ber::BOOLEAN) {
            critical = fields.boolean()?;
        }
        if fields.has_tag(ber::OCTET_STRING) {
            value = Some(fields.octet_string()?.to_vec());
        }

        Ok(Control { oid, critical, value })
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// How far below the base DN a search reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// The base entry itself and nothing else.
    Base,
    /// The base's immediate children, but not the base.
    OneLevel,
    /// The base and everything under it. The default, and what a user lookup
    /// almost always wants: people are rarely all at one depth.
    #[default]
    Subtree,
}

impl Scope {
    pub fn as_i64(self) -> i64 {
        match self {
            Scope::Base => 0,
            Scope::OneLevel => 1,
            Scope::Subtree => 2,
        }
    }
}

/// What to do with alias entries encountered during a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DerefAliases {
    /// Follow nothing. The default here, and deliberately not the protocol's
    /// most permissive option: an alias is an indirection somebody else
    /// controls, and following one during an authentication lookup means the
    /// entry you bind against need not be the entry you searched for.
    #[default]
    Never,
    InSearching,
    FindingBaseObject,
    Always,
}

impl DerefAliases {
    pub fn as_i64(self) -> i64 {
        match self {
            DerefAliases::Never => 0,
            DerefAliases::InSearching => 1,
            DerefAliases::FindingBaseObject => 2,
            DerefAliases::Always => 3,
        }
    }
}

/// A `SearchRequest`, with the defaults a lookup usually wants already set.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub base: String,
    pub scope: Scope,
    pub deref: DerefAliases,
    /// Zero means "no client-side limit"; the directory still has its own.
    pub size_limit: i64,
    /// Seconds. Zero means no client-side limit.
    pub time_limit: i64,
    /// Return attribute names without their values.
    pub types_only: bool,
    pub filter: Filter,
    /// Empty asks for all user attributes. To ask for none, use the OID `1.1`,
    /// which RFC 4511 reserves to mean exactly that.
    pub attributes: Vec<String>,
}

impl SearchRequest {
    pub fn new(base: impl Into<String>, filter: Filter) -> SearchRequest {
        SearchRequest {
            base: base.into(),
            scope: Scope::default(),
            deref: DerefAliases::default(),
            size_limit: 0,
            time_limit: 0,
            types_only: false,
            filter,
            attributes: Vec::new(),
        }
    }

    pub fn scope(mut self, scope: Scope) -> SearchRequest {
        self.scope = scope;
        self
    }

    pub fn deref(mut self, deref: DerefAliases) -> SearchRequest {
        self.deref = deref;
        self
    }

    pub fn size_limit(mut self, limit: i64) -> SearchRequest {
        self.size_limit = limit;
        self
    }

    pub fn time_limit(mut self, seconds: i64) -> SearchRequest {
        self.time_limit = seconds;
        self
    }

    pub fn types_only(mut self, types_only: bool) -> SearchRequest {
        self.types_only = types_only;
        self
    }

    pub fn attributes<S: Into<String>>(
        mut self,
        attributes: impl IntoIterator<Item = S>,
    ) -> SearchRequest {
        self.attributes = attributes.into_iter().map(Into::into).collect();
        self
    }

    fn encode(&self, encoder: &mut Encoder) {
        encoder.constructed(tags::SEARCH_REQUEST, |body| {
            body.string(&self.base);
            body.enumerated(self.scope.as_i64());
            body.enumerated(self.deref.as_i64());
            body.integer(self.size_limit);
            body.integer(self.time_limit);
            body.boolean(self.types_only);
            self.filter.encode(body);
            body.sequence(|list| {
                for attribute in &self.attributes {
                    list.string(attribute);
                }
            });
        });
    }
}

/// One attribute of an entry, with every value the directory returned.
///
/// Values are octets, not strings. `userCertificate` and `objectGUID` are not
/// text, and a type that pretends otherwise loses them.
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: String,
    pub values: Vec<Vec<u8>>,
}

impl Attribute {
    /// The first value as text, if it is text.
    pub fn first(&self) -> Option<&str> {
        self.values.first().and_then(|value| std::str::from_utf8(value).ok())
    }
}

/// A `SearchResultEntry`: one matching entry and its attributes.
#[derive(Debug, Clone)]
pub struct SearchEntry {
    pub dn: String,
    pub attributes: Vec<Attribute>,
}

impl SearchEntry {
    /// Look an attribute up by name, case-insensitively.
    ///
    /// Attribute descriptions are case-insensitive in LDAP, and directories
    /// disagree about which case they return: OpenLDAP echoes what the schema
    /// says, Active Directory echoes what you asked for. Matching exactly is a
    /// bug that only shows up against the other server.
    pub fn attribute(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|attribute| attribute.name.eq_ignore_ascii_case(name))
    }

    /// The first value of an attribute, as text.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.attribute(name).and_then(Attribute::first)
    }

    /// Every value of an attribute that is valid text.
    pub fn values(&self, name: &str) -> Vec<&str> {
        self.attribute(name)
            .map(|attribute| {
                attribute
                    .values
                    .iter()
                    .filter_map(|value| std::str::from_utf8(value).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn decode(decoder: &mut Decoder<'_>) -> Result<SearchEntry> {
        let dn = decoder.string()?;
        let mut attributes = Vec::new();

        let mut list = decoder.sequence()?;
        while !list.is_empty() {
            let mut partial = list.sequence()?;
            let name = partial.string()?;
            let mut values = Vec::new();
            let mut set = partial.set()?;
            while !set.is_empty() {
                values.push(set.octet_string()?.to_vec());
            }
            attributes.push(Attribute { name, values });
        }

        Ok(SearchEntry { dn, attributes })
    }
}

/// Everything one search produced.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub entries: Vec<SearchEntry>,
    /// Continuation references. This client does not chase them: following a
    /// referral means binding somewhere the caller never named, with
    /// credentials the caller never agreed to send there.
    pub referrals: Vec<Vec<String>>,
    pub result: LdapResult,
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// A search filter, built as a value rather than pasted together as a string.
///
/// This is the security-relevant type in the package. The classic LDAP
/// injection is a filter template — `(&(objectClass=person)(uid=NAME))` — with
/// a username interpolated into it: a username of `*` turns the lookup into a
/// wildcard that matches every account, and one containing `)(uid=admin` re-
/// writes the filter outright.
///
/// A `Filter` cannot be injected into, because it never becomes a string on the
/// way to the directory. [`Filter::encode`] writes each assertion value as a
/// length-delimited BER octet string, where `*` and `)` are just bytes with no
/// syntax of their own.
///
/// [`Display`](std::fmt::Display) does produce RFC 4515 text — for logs, and
/// for anything that genuinely needs the string form — and escapes every value
/// on the way out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// `(&(a)(b))`. An empty `And` is the absolute-true filter `(&)` of
    /// RFC 4526, which is well defined and matches everything.
    And(Vec<Filter>),
    /// `(|(a)(b))`. An empty `Or` is RFC 4526's absolute-false filter `(|)`.
    Or(Vec<Filter>),
    Not(Box<Filter>),
    /// `(attribute=value)`
    Equality { attribute: String, value: String },
    /// `(attribute>=value)`
    GreaterOrEqual { attribute: String, value: String },
    /// `(attribute<=value)`
    LessOrEqual { attribute: String, value: String },
    /// `(attribute~=value)`
    Approximate { attribute: String, value: String },
    /// `(attribute=*)` — the entry has this attribute at all.
    Present { attribute: String },
    /// `(attribute=initial*any*any*last)`. RFC 4511 calls the trailing part
    /// `final`, which is a Rust keyword, so it is `last` here.
    Substrings {
        attribute: String,
        initial: Option<String>,
        any: Vec<String>,
        last: Option<String>,
    },
}

impl Filter {
    pub fn equals(attribute: impl Into<String>, value: impl Into<String>) -> Filter {
        Filter::Equality { attribute: attribute.into(), value: value.into() }
    }

    pub fn greater_or_equal(attribute: impl Into<String>, value: impl Into<String>) -> Filter {
        Filter::GreaterOrEqual { attribute: attribute.into(), value: value.into() }
    }

    pub fn less_or_equal(attribute: impl Into<String>, value: impl Into<String>) -> Filter {
        Filter::LessOrEqual { attribute: attribute.into(), value: value.into() }
    }

    pub fn approximately(attribute: impl Into<String>, value: impl Into<String>) -> Filter {
        Filter::Approximate { attribute: attribute.into(), value: value.into() }
    }

    pub fn present(attribute: impl Into<String>) -> Filter {
        Filter::Present { attribute: attribute.into() }
    }

    pub fn starts_with(attribute: impl Into<String>, prefix: impl Into<String>) -> Filter {
        Filter::Substrings {
            attribute: attribute.into(),
            initial: Some(prefix.into()),
            any: Vec::new(),
            last: None,
        }
    }

    pub fn ends_with(attribute: impl Into<String>, suffix: impl Into<String>) -> Filter {
        Filter::Substrings {
            attribute: attribute.into(),
            initial: None,
            any: Vec::new(),
            last: Some(suffix.into()),
        }
    }

    pub fn contains(attribute: impl Into<String>, fragment: impl Into<String>) -> Filter {
        Filter::Substrings {
            attribute: attribute.into(),
            initial: None,
            any: vec![fragment.into()],
            last: None,
        }
    }

    pub fn and(filters: impl IntoIterator<Item = Filter>) -> Filter {
        Filter::And(filters.into_iter().collect())
    }

    pub fn or(filters: impl IntoIterator<Item = Filter>) -> Filter {
        Filter::Or(filters.into_iter().collect())
    }

    /// `(!(…))`. Also spelled `!filter`, through [`std::ops::Not`]; this form
    /// exists because it chains, and a filter is usually built left to right.
    pub fn negate(self) -> Filter {
        Filter::Not(Box::new(self))
    }

    /// Write the filter as BER, per RFC 4511 §4.5.1.
    pub fn encode(&self, encoder: &mut Encoder) {
        match self {
            Filter::And(filters) => {
                encoder.constructed(ber::context(0, true), |body| {
                    for filter in filters {
                        filter.encode(body);
                    }
                });
            }
            Filter::Or(filters) => {
                encoder.constructed(ber::context(1, true), |body| {
                    for filter in filters {
                        filter.encode(body);
                    }
                });
            }
            // [2] wraps a CHOICE, and a tag on a CHOICE is always explicit — so
            // the contents here are a whole inner filter, tag and all.
            Filter::Not(inner) => {
                encoder.constructed(ber::context(2, true), |body| inner.encode(body));
            }
            Filter::Equality { attribute, value } => {
                assertion(encoder, ber::context(3, true), attribute, value);
            }
            Filter::Substrings { attribute, initial, any, last } => {
                encoder.constructed(ber::context(4, true), |body| {
                    body.string(attribute);
                    body.sequence(|parts| {
                        if let Some(initial) = initial {
                            parts.tagged_string(ber::context(0, false), initial);
                        }
                        for fragment in any {
                            parts.tagged_string(ber::context(1, false), fragment);
                        }
                        if let Some(last) = last {
                            parts.tagged_string(ber::context(2, false), last);
                        }
                    });
                });
            }
            Filter::GreaterOrEqual { attribute, value } => {
                assertion(encoder, ber::context(5, true), attribute, value);
            }
            Filter::LessOrEqual { attribute, value } => {
                assertion(encoder, ber::context(6, true), attribute, value);
            }
            // [7] is primitive: the attribute description *is* the contents.
            Filter::Present { attribute } => {
                encoder.tagged_string(ber::context(7, false), attribute);
            }
            Filter::Approximate { attribute, value } => {
                assertion(encoder, ber::context(8, true), attribute, value);
            }
        }
    }
}

/// `!filter`, for the times that reads better than [`Filter::negate`].
impl std::ops::Not for Filter {
    type Output = Filter;

    fn not(self) -> Filter {
        Filter::Not(Box::new(self))
    }
}

fn assertion(encoder: &mut Encoder, tag: u8, attribute: &str, value: &str) {
    encoder.constructed(tag, |body| {
        body.string(attribute);
        body.string(value);
    });
}

impl std::fmt::Display for Filter {
    /// The RFC 4515 string form, with every value escaped.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Filter::And(filters) => {
                f.write_str("(&")?;
                for filter in filters {
                    write!(f, "{filter}")?;
                }
                f.write_str(")")
            }
            Filter::Or(filters) => {
                f.write_str("(|")?;
                for filter in filters {
                    write!(f, "{filter}")?;
                }
                f.write_str(")")
            }
            Filter::Not(inner) => write!(f, "(!{inner})"),
            Filter::Equality { attribute, value } => {
                write!(f, "({}={})", escape_filter_value(attribute), escape_filter_value(value))
            }
            Filter::GreaterOrEqual { attribute, value } => {
                write!(f, "({}>={})", escape_filter_value(attribute), escape_filter_value(value))
            }
            Filter::LessOrEqual { attribute, value } => {
                write!(f, "({}<={})", escape_filter_value(attribute), escape_filter_value(value))
            }
            Filter::Approximate { attribute, value } => {
                write!(f, "({}~={})", escape_filter_value(attribute), escape_filter_value(value))
            }
            Filter::Present { attribute } => write!(f, "({}=*)", escape_filter_value(attribute)),
            Filter::Substrings { attribute, initial, any, last } => {
                write!(f, "({}=", escape_filter_value(attribute))?;
                if let Some(initial) = initial {
                    f.write_str(&escape_filter_value(initial))?;
                }
                f.write_str("*")?;
                for fragment in any {
                    f.write_str(&escape_filter_value(fragment))?;
                    f.write_str("*")?;
                }
                if let Some(last) = last {
                    f.write_str(&escape_filter_value(last))?;
                }
                f.write_str(")")
            }
        }
    }
}

/// Escape a value for the RFC 4515 string form of a filter.
///
/// RFC 4515 §3 requires five characters to be escaped as `\` plus two hex
/// digits: the backslash itself, the asterisk, both parentheses, and NUL. The
/// asterisk is the one that matters. A username of `*` pasted unescaped into
/// `(uid=NAME)` produces `(uid=*)`, which matches every account in the
/// directory — and a lookup that returns the first of them is a lookup that has
/// just chosen somebody else's account for the person logging in.
///
/// Use this whenever a value has to reach a filter through text. Values that go
/// through [`Filter`] and [`Filter::encode`] need no escaping at all, because
/// BER's octet strings carry their own length and have no metacharacters — that
/// is the reason the typed builder exists.
pub fn escape_filter_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\5c"),
            '*' => escaped.push_str("\\2a"),
            '(' => escaped.push_str("\\28"),
            ')' => escaped.push_str("\\29"),
            '\0' => escaped.push_str("\\00"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Escape a value for use inside a distinguished name (RFC 4514 §2.4).
///
/// The set is different from the filter one, and so is the risk: a DN is what a
/// bind is *for*, so a username smuggled into `uid=NAME,ou=people,dc=example`
/// with a comma in it binds against an entry the template never described.
///
/// A leading `#` or a leading or trailing space also have to be escaped, and
/// this handles those with a backslash before the character rather than a hex
/// escape, which RFC 4514 allows and which stays readable in a log.
pub fn escape_dn_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    let characters: Vec<char> = value.chars().collect();

    for (index, &character) in characters.iter().enumerate() {
        let first = index == 0;
        let last = index + 1 == characters.len();

        match character {
            '\0' => escaped.push_str("\\00"),
            '"' | '+' | ',' | ';' | '<' | '>' | '\\' | '=' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '#' if first => escaped.push_str("\\#"),
            ' ' if first || last => escaped.push_str("\\ "),
            other => escaped.push(other),
        }
    }

    escaped
}

// ---------------------------------------------------------------------------
// Outgoing operations
// ---------------------------------------------------------------------------

/// One encoded protocol operation, waiting for a message id.
///
/// **No `Debug`.** A `simple_bind` operation's bytes contain the password in
/// the clear — that is what a simple bind *is* — and a struct that can be
/// printed is a struct that eventually gets printed into a log.
pub struct Operation {
    kind: &'static str,
    op: Vec<u8>,
    controls: Vec<Control>,
    /// Whether sending this puts a password on the wire. The connection checks
    /// it before writing anything, so the transport rule cannot be forgotten at
    /// a call site.
    carries_password: bool,
}

impl Operation {
    /// A simple bind: a DN and a password, both in the clear.
    ///
    /// # An empty password is an anonymous bind
    ///
    /// RFC 4511 §4.2 is explicit: a simple bind with a zero-length password is
    /// an *unauthenticated* bind, and a directory answers it with `success`. So
    /// a login form whose password field was left empty, passed straight
    /// through, comes back as "authenticated" — for any username that exists.
    /// This has been a real vulnerability in real products more than once.
    ///
    /// So it is refused here, at the point the bytes are built, rather than
    /// anywhere it could be skipped. A caller that genuinely wants an anonymous
    /// bind asks for one by name with [`Operation::anonymous_bind`].
    pub fn simple_bind(dn: &str, password: &str) -> Result<Operation> {
        if password.is_empty() {
            return Err(Error::msg(
                "refusing to send a simple bind with an empty password. LDAP treats that as an \
                 anonymous bind and answers `success`, so a blank password field would report a \
                 successful login. If an anonymous bind is what you meant, ask for one by name \
                 with `bind_anonymous`.",
            ));
        }

        Ok(Operation {
            kind: "bind",
            op: bind_bytes(dn, password),
            controls: Vec::new(),
            carries_password: true,
        })
    }

    /// An anonymous bind: no name, no password, no authentication.
    ///
    /// A legitimate operation — many directories allow an anonymous search —
    /// and spelled out here so that it can never be reached by accident.
    pub fn anonymous_bind() -> Operation {
        Operation {
            kind: "anonymous bind",
            op: bind_bytes("", ""),
            controls: Vec::new(),
            carries_password: false,
        }
    }

    pub fn search(request: &SearchRequest) -> Operation {
        let mut encoder = Encoder::new();
        request.encode(&mut encoder);
        Operation {
            kind: "search",
            op: encoder.into_bytes(),
            controls: Vec::new(),
            carries_password: false,
        }
    }

    /// `UnbindRequest ::= [APPLICATION 2] NULL`. There is no unbind *response*:
    /// it means "I am closing this connection", not "please reply".
    pub fn unbind() -> Operation {
        let mut encoder = Encoder::new();
        encoder.element(tags::UNBIND_REQUEST, &[]);
        Operation {
            kind: "unbind",
            op: encoder.into_bytes(),
            controls: Vec::new(),
            carries_password: false,
        }
    }

    pub fn extended(oid: &str, value: Option<&[u8]>) -> Operation {
        let mut encoder = Encoder::new();
        encoder.constructed(tags::EXTENDED_REQUEST, |body| {
            body.tagged_string(ber::context(0, false), oid);
            if let Some(value) = value {
                body.element(ber::context(1, false), value);
            }
        });
        Operation {
            kind: "extended request",
            op: encoder.into_bytes(),
            controls: Vec::new(),
            carries_password: false,
        }
    }

    pub fn start_tls() -> Operation {
        let mut operation = Operation::extended(START_TLS_OID, None);
        operation.kind = "StartTLS request";
        operation
    }

    pub fn with_control(mut self, control: Control) -> Operation {
        self.controls.push(control);
        self
    }

    pub fn kind(&self) -> &'static str {
        self.kind
    }

    pub fn carries_password(&self) -> bool {
        self.carries_password
    }

    /// Wrap the operation in an `LDAPMessage` with the given id.
    pub fn encode(&self, message_id: i64) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.sequence(|body| {
            body.integer(message_id);
            body.raw(&self.op);
            if !self.controls.is_empty() {
                body.constructed(ber::context(0, true), |list| {
                    for control in &self.controls {
                        control.encode(list);
                    }
                });
            }
        });
        encoder.into_bytes()
    }
}

fn bind_bytes(dn: &str, password: &str) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.constructed(tags::BIND_REQUEST, |body| {
        body.integer(VERSION);
        body.string(dn);
        // AuthenticationChoice ::= CHOICE { simple [0] OCTET STRING, ... }
        body.tagged_string(ber::context(0, false), password);
    });
    encoder.into_bytes()
}

// ---------------------------------------------------------------------------
// Incoming messages
// ---------------------------------------------------------------------------

/// A decoded protocol operation, in the shapes this client asks for.
#[derive(Debug, Clone)]
pub enum ProtocolOp {
    BindResponse { result: LdapResult, server_sasl_credentials: Option<Vec<u8>> },
    SearchResultEntry(SearchEntry),
    SearchResultReference(Vec<String>),
    SearchResultDone(LdapResult),
    ExtendedResponse { result: LdapResult, name: Option<String>, value: Option<Vec<u8>> },
    IntermediateResponse,
    /// Something this client never asked for. The tag is kept so the error can
    /// name it; the contents are dropped, because guessing at the shape of a
    /// message you did not request is how a parser gets surprised.
    Unrecognised { tag: u8 },
}

impl ProtocolOp {
    /// The `LDAPResult` inside, for the operations that carry one.
    pub fn result(&self) -> Option<&LdapResult> {
        match self {
            ProtocolOp::BindResponse { result, .. }
            | ProtocolOp::SearchResultDone(result)
            | ProtocolOp::ExtendedResponse { result, .. } => Some(result),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            ProtocolOp::BindResponse { .. } => "a bind response",
            ProtocolOp::SearchResultEntry(_) => "a search result entry",
            ProtocolOp::SearchResultReference(_) => "a search result reference",
            ProtocolOp::SearchResultDone(_) => "a search result",
            ProtocolOp::ExtendedResponse { .. } => "an extended response",
            ProtocolOp::IntermediateResponse => "an intermediate response",
            ProtocolOp::Unrecognised { .. } => "an operation this client does not implement",
        }
    }
}

/// One `LDAPMessage` off the wire.
#[derive(Debug, Clone)]
pub struct LdapMessage {
    /// Zero means an unsolicited notification (RFC 4511 §4.4) — the directory
    /// speaking without being asked, which in practice means it is about to
    /// close the connection.
    pub id: i64,
    pub op: ProtocolOp,
    pub controls: Vec<Control>,
}

impl LdapMessage {
    /// Parse exactly one message. The bytes must be one complete element, which
    /// is what [`crate::ber::element_size`] is for.
    pub fn parse(bytes: &[u8]) -> Result<LdapMessage> {
        let mut outer = Decoder::new(bytes);
        let mut message = outer.sequence()?;
        outer.finish("an LDAP message")?;

        let id = message.integer()?;
        if id < 0 {
            return Err(Error::msg(format!(
                "the directory sent message id {id}; RFC 4511 says a message id is a positive \
                 integer or zero"
            )));
        }

        let tag = message
            .peek_tag()
            .ok_or_else(|| Error::msg("an LDAP message carried an id but no operation"))?;

        let op = match tag {
            tags::BIND_RESPONSE => {
                let mut fields = message.nested(tag)?;
                let result = LdapResult::decode(&mut fields)?;
                let credentials = optional_octets(&mut fields, ber::context(7, false))?;
                ProtocolOp::BindResponse { result, server_sasl_credentials: credentials }
            }
            tags::SEARCH_RESULT_ENTRY => {
                let mut fields = message.nested(tag)?;
                ProtocolOp::SearchResultEntry(SearchEntry::decode(&mut fields)?)
            }
            tags::SEARCH_RESULT_DONE => {
                let mut fields = message.nested(tag)?;
                ProtocolOp::SearchResultDone(LdapResult::decode(&mut fields)?)
            }
            tags::SEARCH_RESULT_REFERENCE => {
                let mut fields = message.nested(tag)?;
                let mut uris = Vec::new();
                while !fields.is_empty() {
                    uris.push(fields.string()?);
                }
                ProtocolOp::SearchResultReference(uris)
            }
            tags::EXTENDED_RESPONSE => {
                let mut fields = message.nested(tag)?;
                let result = LdapResult::decode(&mut fields)?;
                let name = match fields.has_tag(ber::context(10, false)) {
                    true => Some(fields.tagged_string(ber::context(10, false))?),
                    false => None,
                };
                let value = optional_octets(&mut fields, ber::context(11, false))?;
                ProtocolOp::ExtendedResponse { result, name, value }
            }
            tags::INTERMEDIATE_RESPONSE => {
                message.skip()?;
                ProtocolOp::IntermediateResponse
            }
            other => {
                message.skip()?;
                ProtocolOp::Unrecognised { tag: other }
            }
        };

        let mut controls = Vec::new();
        if message.has_tag(ber::context(0, true)) {
            let mut list = message.nested(ber::context(0, true))?;
            while !list.is_empty() {
                controls.push(Control::decode(&mut list)?);
            }
        }

        message.finish("an LDAP message")?;
        Ok(LdapMessage { id, op, controls })
    }
}

fn optional_octets(decoder: &mut Decoder<'_>, tag: u8) -> Result<Option<Vec<u8>>> {
    if decoder.has_tag(tag) {
        return Ok(Some(decoder.expect(tag)?.to_vec()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_bytes(filter: &Filter) -> Vec<u8> {
        let mut encoder = Encoder::new();
        filter.encode(&mut encoder);
        encoder.into_bytes()
    }

    #[test]
    fn an_anonymous_bind_is_the_message_every_reference_shows() {
        // The canonical LDAPMessage: id 1, BindRequest, version 3, empty name,
        // empty simple password. Fourteen bytes, and every one of them is
        // checked here because this is the message that proves the framing,
        // the application tag and the implicit context tag are all right.
        assert_eq!(
            Operation::anonymous_bind().encode(1),
            vec![
                0x30, 0x0c, // SEQUENCE, 12 bytes
                0x02, 0x01, 0x01, // messageID 1
                0x60, 0x07, // [APPLICATION 0] BindRequest, 7 bytes
                0x02, 0x01, 0x03, // version 3
                0x04, 0x00, // name ""
                0x80, 0x00, // simple [0] ""
            ]
        );
    }

    #[test]
    fn a_simple_bind_carries_the_dn_and_the_password_unwrapped() {
        let bytes = Operation::simple_bind("cn=root", "hunter2").unwrap().encode(2);

        assert_eq!(
            bytes,
            vec![
                0x30, 0x1a, //
                0x02, 0x01, 0x02, // messageID 2
                0x60, 0x15, // BindRequest
                0x02, 0x01, 0x03, // version 3
                0x04, 0x07, b'c', b'n', b'=', b'r', b'o', b'o', b't', //
                0x80, 0x07, b'h', b'u', b'n', b't', b'e', b'r', b'2',
            ]
        );

        // Which is the whole reason the transport rule exists: the password is
        // right there, one hexdump away from anyone on the path.
        assert!(bytes.windows(7).any(|window| window == b"hunter2"));
        assert!(Operation::simple_bind("cn=root", "hunter2").unwrap().carries_password());
        assert!(!Operation::anonymous_bind().carries_password());
    }

    #[test]
    fn an_empty_password_is_refused_before_it_reaches_the_wire() {
        // LDAP answers a simple bind with a zero-length password with
        // `success`, because the specification says that is an anonymous bind.
        // A login form that passes an empty password field straight through
        // therefore reports a successful login — which is the bug, and it has
        // shipped in real products.
        //
        // The refusal lives in the encoder rather than in a caller, so there is
        // no code path that reaches the socket without passing this.
        // `Operation` has no `Debug` — its bytes hold the password — so the
        // error comes out of a match rather than `unwrap_err`.
        let error = match Operation::simple_bind("uid=alice,dc=example,dc=test", "") {
            Ok(_) => panic!("an empty password must never build a bind"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("anonymous"), "the error must say what an empty password becomes");
        assert!(error.contains("bind_anonymous"), "and name the deliberate alternative: {error}");

        // A password of one space is a password. Only zero length is the trap.
        assert!(Operation::simple_bind("uid=alice,dc=example,dc=test", " ").is_ok());
    }

    #[test]
    fn unbind_is_a_primitive_null_with_no_response() {
        assert_eq!(Operation::unbind().encode(3), vec![0x30, 0x05, 0x02, 0x01, 0x03, 0x42, 0x00]);
    }

    #[test]
    fn start_tls_names_the_oid_from_rfc_4511() {
        let bytes = Operation::start_tls().encode(1);
        let oid = START_TLS_OID.as_bytes();

        assert_eq!(&bytes[..5], &[0x30, 0x1d, 0x02, 0x01, 0x01]);
        assert_eq!(bytes[5], 0x77, "[APPLICATION 23] ExtendedRequest");
        assert_eq!(bytes[7], 0x80, "requestName [0], primitive");
        assert_eq!(bytes[8] as usize, oid.len());
        assert_eq!(&bytes[9..], oid);
    }

    #[test]
    fn an_equality_filter_puts_the_value_in_a_length_delimited_string() {
        // (uid=alice): [3] AttributeValueAssertion, two octet strings.
        assert_eq!(
            filter_bytes(&Filter::equals("uid", "alice")),
            vec![
                0xa3, 0x0c, //
                0x04, 0x03, b'u', b'i', b'd', //
                0x04, 0x05, b'a', b'l', b'i', b'c', b'e',
            ]
        );
    }

    #[test]
    fn an_asterisk_in_an_equality_value_stays_a_literal_asterisk() {
        // This is the property the whole typed-filter design exists for. In the
        // string form `(uid=*)` is a presence filter that matches every entry.
        // In BER it is an equality assertion whose value is one byte, 0x2a, and
        // a directory compares it as a byte: it matches the account literally
        // named `*`, which is to say nothing.
        assert_eq!(
            filter_bytes(&Filter::equals("uid", "*")),
            vec![0xa3, 0x08, 0x04, 0x03, b'u', b'i', b'd', 0x04, 0x01, 0x2a]
        );

        // And a presence filter, which is what `(uid=*)` means in text, is a
        // different tag entirely — you cannot arrive at it by accident.
        assert_eq!(
            filter_bytes(&Filter::present("uid")),
            vec![0x87, 0x03, b'u', b'i', b'd']
        );

        // Nor can an injected filter fragment escape its own octet string.
        let injected = Filter::equals("uid", "alice)(uid=admin");
        let bytes = filter_bytes(&injected);
        assert_eq!(bytes[0], 0xa3, "still one equality assertion");
        assert_eq!(bytes.iter().filter(|&&byte| byte == 0xa3).count(), 1);
    }

    #[test]
    fn a_not_filter_wraps_a_whole_inner_filter_because_the_tag_is_explicit() {
        // [2] tags a CHOICE, and a tag on a CHOICE is always explicit. Getting
        // this wrong produces bytes a directory answers with protocolError.
        assert_eq!(
            filter_bytes(&!Filter::equals("cn", "x")),
            vec![0xa2, 0x09, 0xa3, 0x07, 0x04, 0x02, b'c', b'n', 0x04, 0x01, b'x']
        );
    }

    #[test]
    fn and_and_or_replace_the_set_tag_rather_than_wrapping_it() {
        // IMPLICIT TAGS: [0] SET OF Filter is 0xa0 and there is no 0x31 inside.
        let bytes = filter_bytes(&Filter::and([Filter::present("a"), Filter::present("b")]));
        assert_eq!(bytes, vec![0xa0, 0x06, 0x87, 0x01, b'a', 0x87, 0x01, b'b']);

        let bytes = filter_bytes(&Filter::or([Filter::present("a")]));
        assert_eq!(bytes, vec![0xa1, 0x03, 0x87, 0x01, b'a']);
    }

    #[test]
    fn a_substring_filter_tags_each_part_by_position() {
        // (o=univ*of*mich*) — RFC 4515's own example, in BER.
        let filter = Filter::Substrings {
            attribute: "o".into(),
            initial: Some("univ".into()),
            any: vec!["of".into(), "mich".into()],
            last: None,
        };

        assert_eq!(
            filter_bytes(&filter),
            vec![
                0xa4, 0x15, //
                0x04, 0x01, b'o', //
                0x30, 0x10, // SEQUENCE OF substring
                0x80, 0x04, b'u', b'n', b'i', b'v', // initial [0]
                0x81, 0x02, b'o', b'f', // any [1]
                0x81, 0x04, b'm', b'i', b'c', b'h', // any [1]
            ]
        );
        assert_eq!(filter.to_string(), "(o=univ*of*mich*)");
    }

    #[test]
    fn filters_print_the_examples_from_rfc_4515() {
        // Section 4 of RFC 4515, which is the only place the string form is
        // written down with worked examples.
        assert_eq!(Filter::equals("cn", "Babs Jensen").to_string(), "(cn=Babs Jensen)");
        assert_eq!(Filter::equals("cn", "Tim Howes").negate().to_string(), "(!(cn=Tim Howes))");
        // The two spellings are the same filter.
        assert_eq!(!Filter::equals("cn", "Tim Howes"), Filter::equals("cn", "Tim Howes").negate());
        assert_eq!(
            Filter::and([
                Filter::equals("objectClass", "Person"),
                Filter::or([
                    Filter::equals("sn", "Jensen"),
                    Filter::starts_with("cn", "Babs J"),
                ]),
            ])
            .to_string(),
            "(&(objectClass=Person)(|(sn=Jensen)(cn=Babs J*)))"
        );
        assert_eq!(Filter::equals("seeAlso", "").to_string(), "(seeAlso=)");
        assert_eq!(Filter::present("objectClass").to_string(), "(objectClass=*)");

        // RFC 4526's absolute true and false filters.
        assert_eq!(Filter::and([]).to_string(), "(&)");
        assert_eq!(Filter::or([]).to_string(), "(|)");
    }

    #[test]
    fn escaping_matches_rfc_4515_s_worked_examples() {
        // Every one of these is verbatim from RFC 4515 §4.
        assert_eq!(
            Filter::equals("o", "Parens R Us (for all your parenthetical needs)").to_string(),
            "(o=Parens R Us \\28for all your parenthetical needs\\29)"
        );
        assert_eq!(Filter::contains("cn", "*").to_string(), "(cn=*\\2a*)");
        assert_eq!(
            Filter::equals("filename", "C:\\MyFile").to_string(),
            "(filename=C:\\5cMyFile)"
        );
        // RFC 4515's `(bin=\00\00\00\04)`. The NULs must be escaped; the 0x04
        // may be, and this escapes only what the grammar forbids raw.
        assert_eq!(escape_filter_value("\0\0\0\u{4}"), "\\00\\00\\00\u{4}");
    }

    #[test]
    fn an_unescaped_asterisk_would_have_matched_everybody() {
        // The bug, stated as a test. Pasting a username into a template is what
        // real code does, and `*` is a username anyone can type.
        let username = "*";
        let naive = format!("(uid={username})");
        assert_eq!(naive, "(uid=*)", "which is a presence filter — every account matches");

        let escaped = format!("(uid={})", escape_filter_value(username));
        assert_eq!(escaped, "(uid=\\2a)", "which matches an account literally named *");

        // And the same for the injection that rewrites the filter outright.
        assert_eq!(
            escape_filter_value("alice)(uid=admin"),
            "alice\\29\\28uid=admin",
            "the parentheses have to go, or the filter grows a second clause"
        );
    }

    #[test]
    fn dn_escaping_covers_what_rfc_4514_requires() {
        assert_eq!(escape_dn_value("Smith, John"), "Smith\\, John");
        assert_eq!(escape_dn_value("a+b"), "a\\+b");
        assert_eq!(escape_dn_value("back\\slash"), "back\\\\slash");
        // Leading and trailing spaces, and a leading hash, are positional.
        assert_eq!(escape_dn_value(" alice "), "\\ alice\\ ");
        assert_eq!(escape_dn_value("#alice"), "\\#alice");
        assert_eq!(escape_dn_value("mid#dle"), "mid#dle");
        // The injection this prevents: a comma ends the RDN, so an unescaped
        // one moves the bind to an entry the template never described.
        assert_eq!(
            escape_dn_value("alice,ou=admins"),
            "alice\\,ou\\=admins"
        );
    }

    #[test]
    fn a_search_request_encodes_every_field_in_order() {
        let request = SearchRequest::new("dc=example,dc=test", Filter::equals("uid", "alice"))
            .scope(Scope::Subtree)
            .size_limit(2)
            .attributes(["cn"]);

        assert_eq!(
            Operation::search(&request).encode(4),
            vec![
                0x30, 0x3c, //
                0x02, 0x01, 0x04, // messageID 4
                0x63, 0x37, // [APPLICATION 3] SearchRequest
                0x04, 0x12, b'd', b'c', b'=', b'e', b'x', b'a', b'm', b'p', b'l', b'e', b',',
                b'd', b'c', b'=', b't', b'e', b's', b't', //
                0x0a, 0x01, 0x02, // scope wholeSubtree
                0x0a, 0x01, 0x00, // derefAliases neverDerefAliases
                0x02, 0x01, 0x02, // sizeLimit 2
                0x02, 0x01, 0x00, // timeLimit 0
                0x01, 0x01, 0x00, // typesOnly FALSE
                0xa3, 0x0c, 0x04, 0x03, b'u', b'i', b'd', 0x04, 0x05, b'a', b'l', b'i', b'c',
                b'e', // filter
                0x30, 0x04, 0x04, 0x02, b'c', b'n', // attributes
            ]
        );
    }

    #[test]
    fn a_bind_response_decodes_its_code_and_the_directory_s_own_words() {
        // resultCode invalidCredentials, no matched DN, a diagnostic.
        let mut encoder = Encoder::new();
        encoder.sequence(|body| {
            body.integer(2);
            body.constructed(tags::BIND_RESPONSE, |fields| {
                fields.enumerated(49);
                fields.string("");
                fields.string("80090308: LdapErr: DSID-0C0903A9, comment: ..., data 533, v2580");
            });
        });

        let message = LdapMessage::parse(encoder.as_bytes()).unwrap();
        assert_eq!(message.id, 2);

        let ProtocolOp::BindResponse { result, .. } = &message.op else {
            panic!("expected a bind response, got {}", message.op.name());
        };
        assert_eq!(result.code, ResultCode::InvalidCredentials);
        assert!(result.diagnostic.contains("data 533"));
        assert!(!result.is_success());
    }

    #[test]
    fn a_search_result_entry_decodes_its_dn_and_multi_valued_attributes() {
        let mut encoder = Encoder::new();
        encoder.sequence(|body| {
            body.integer(5);
            body.constructed(tags::SEARCH_RESULT_ENTRY, |entry| {
                entry.string("uid=alice,ou=people,dc=example,dc=test");
                entry.sequence(|attributes| {
                    attributes.sequence(|attribute| {
                        attribute.string("cn");
                        attribute.set(|values| {
                            values.octet_string(b"Alice Liddell");
                        });
                    });
                    attributes.sequence(|attribute| {
                        attribute.string("objectClass");
                        attribute.set(|values| {
                            values.octet_string(b"top");
                            values.octet_string(b"inetOrgPerson");
                        });
                    });
                });
            });
        });

        let message = LdapMessage::parse(encoder.as_bytes()).unwrap();
        let ProtocolOp::SearchResultEntry(entry) = &message.op else {
            panic!("expected an entry, got {}", message.op.name());
        };

        assert_eq!(entry.dn, "uid=alice,ou=people,dc=example,dc=test");
        assert_eq!(entry.value("cn"), Some("Alice Liddell"));
        // Attribute descriptions are case-insensitive, and directories differ
        // about which case they send back.
        assert_eq!(entry.value("CN"), Some("Alice Liddell"));
        assert_eq!(entry.values("objectclass"), vec!["top", "inetOrgPerson"]);
        assert_eq!(entry.value("mail"), None);
    }

    #[test]
    fn a_referral_in_a_result_is_read_and_not_followed() {
        let mut encoder = Encoder::new();
        encoder.sequence(|body| {
            body.integer(6);
            body.constructed(tags::SEARCH_RESULT_DONE, |fields| {
                fields.enumerated(10);
                fields.string("dc=example,dc=test");
                fields.string("");
                fields.constructed(ber::context(3, true), |referral| {
                    referral.string("ldap://other.example.test/dc=example,dc=test");
                });
            });
        });

        let message = LdapMessage::parse(encoder.as_bytes()).unwrap();
        let ProtocolOp::SearchResultDone(result) = &message.op else {
            panic!("expected a search result");
        };
        assert_eq!(result.code, ResultCode::Referral);
        assert_eq!(result.referrals, vec!["ldap://other.example.test/dc=example,dc=test"]);
        assert_eq!(result.matched_dn, "dc=example,dc=test");
    }

    #[test]
    fn controls_round_trip_on_a_request_and_a_response() {
        let operation = Operation::search(&SearchRequest::new("", Filter::present("objectClass")))
            .with_control(Control::new("1.2.840.113556.1.4.319").critical(true).value(vec![1, 2]));

        // The controls ride in [0] on the message, after the operation. Parsed
        // back, the search request itself is an operation this client does not
        // decode — which is exactly the case where the controls must still be
        // found, since they are read positionally after the operation.
        let bytes = operation.encode(9);
        let parsed = LdapMessage::parse(&bytes).unwrap();
        assert_eq!(parsed.id, 9);
        assert!(matches!(parsed.op, ProtocolOp::Unrecognised { .. }));
        assert_eq!(parsed.controls.len(), 1);
        assert_eq!(parsed.controls[0].oid, "1.2.840.113556.1.4.319");
        assert!(parsed.controls[0].critical);
        assert_eq!(parsed.controls[0].value, Some(vec![1, 2]));

        let mut encoder = Encoder::new();
        encoder.sequence(|body| {
            body.integer(9);
            body.constructed(tags::SEARCH_RESULT_DONE, |fields| {
                fields.enumerated(0);
                fields.string("");
                fields.string("");
            });
            body.constructed(ber::context(0, true), |list| {
                list.sequence(|control| {
                    control.string("1.2.840.113556.1.4.319");
                    control.octet_string(&[7]);
                });
            });
        });

        let message = LdapMessage::parse(encoder.as_bytes()).unwrap();
        assert_eq!(message.controls.len(), 1);
        assert_eq!(message.controls[0].oid, "1.2.840.113556.1.4.319");
        assert!(!message.controls[0].critical, "criticality DEFAULT FALSE when absent");
        assert_eq!(message.controls[0].value, Some(vec![7]));
    }

    #[test]
    fn an_operation_this_client_never_asked_for_is_named_not_guessed_at() {
        let mut encoder = Encoder::new();
        encoder.sequence(|body| {
            body.integer(1);
            // [APPLICATION 7] ModifyResponse — well formed, and not ours.
            body.constructed(ber::application(7, true), |fields| {
                fields.enumerated(0);
                fields.string("");
                fields.string("");
            });
        });

        let message = LdapMessage::parse(encoder.as_bytes()).unwrap();
        assert!(matches!(message.op, ProtocolOp::Unrecognised { tag: 0x67 }));
        assert!(message.op.result().is_none());
    }

    #[test]
    fn a_truncated_or_corrupt_message_is_an_error_and_never_a_panic() {
        let mut encoder = Encoder::new();
        encoder.sequence(|body| {
            body.integer(1);
            body.constructed(tags::BIND_RESPONSE, |fields| {
                fields.enumerated(0);
                fields.string("");
                fields.string("");
            });
        });
        let complete = encoder.into_bytes();

        for length in 0..complete.len() {
            assert!(
                LdapMessage::parse(&complete[..length]).is_err(),
                "a {length} byte prefix should not parse"
            );
        }
        assert!(LdapMessage::parse(&complete).is_ok());

        // Trailing bytes after a complete message are a second message, and
        // this function parses one.
        let mut extra = complete.clone();
        extra.push(0x00);
        assert!(LdapMessage::parse(&extra).is_err());
    }

    #[test]
    fn result_codes_round_trip_and_name_themselves() {
        for code in [0i64, 1, 32, 49, 50, 53, 80] {
            assert_eq!(ResultCode::from_i64(code).as_i64(), code);
        }
        assert_eq!(ResultCode::from_i64(49), ResultCode::InvalidCredentials);
        assert_eq!(ResultCode::from_i64(32), ResultCode::NoSuchObject);
        assert_eq!(ResultCode::from_i64(4242), ResultCode::Unknown(4242));
        assert_eq!(ResultCode::InvalidCredentials.to_string(), "invalidCredentials (49)");
        assert_eq!(ResultCode::Unknown(4242).as_i64(), 4242);
        assert!(ResultCode::Success.is_success());
    }

    #[test]
    fn a_failed_result_explains_itself_in_words() {
        let result = LdapResult {
            code: ResultCode::NoSuchObject,
            matched_dn: "dc=example,dc=test".into(),
            diagnostic: "no such object".into(),
            referrals: Vec::new(),
        };

        let error = result.into_result("search").unwrap_err().to_string();
        assert!(error.contains("noSuchObject (32)"), "got {error}");
        assert!(error.contains("no such object"), "got {error}");
        assert!(error.contains("dc=example,dc=test"), "the matched DN is the clue: {error}");
    }
}
