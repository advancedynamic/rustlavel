//! What registration and authentication agree on.
//!
//! The two ceremonies differ in what the authenticator hands back and in
//! almost nothing else. Both read a `clientDataJSON` and check its type, its
//! challenge and its origin; both read an `authenticatorData` and check the
//! relying party hash and the flags. Writing those checks twice is how the two
//! halves drift apart, and a check present in one ceremony and missing from
//! the other is a hole in whichever one is weaker.
//!
//! So the checks live here once, and each ceremony calls them.

use crate::cbor::Cbor;
use crate::cose::CoseKey;
use rustlavel_auth::base64;
use rustlavel_core::{Config, Error, Json, Result};
use sha2::{Digest, Sha256};

/// SHA-256, the only hash WebAuthn uses.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Read a base64url field out of the JSON a browser posted.
pub(crate) fn base64_field(value: &Json, path: &str) -> Result<Vec<u8>> {
    let text = value
        .get(path)
        .and_then(Json::as_str)
        .ok_or_else(|| Error::msg(format!("the credential has no `{path}`")))?;

    base64::decode(text)
        .ok_or_else(|| Error::msg(format!("`{path}` is not base64url and cannot be decoded")))
}

/// How hard the relying party insists the user prove who they are.
///
/// `Preferred` is the default because it is what the specification defaults
/// to: the authenticator verifies if it can, and an assertion that skipped it
/// is still accepted. Only `Required` turns a missing User Verified flag into
/// a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserVerification {
    Required,
    #[default]
    Preferred,
    Discouraged,
}

impl UserVerification {
    pub fn as_str(self) -> &'static str {
        match self {
            UserVerification::Required => "required",
            UserVerification::Preferred => "preferred",
            UserVerification::Discouraged => "discouraged",
        }
    }

    pub fn is_required(self) -> bool {
        self == UserVerification::Required
    }
}

/// Whether the credential must be discoverable — a passkey, in other words.
///
/// A discoverable credential is stored on the authenticator with the user
/// handle inside it, which is what lets somebody sign in without first typing
/// a username. A non-discoverable one needs the server to name it in
/// `allowCredentials`, so it cannot start a login on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResidentKey {
    Required,
    #[default]
    Preferred,
    Discouraged,
}

impl ResidentKey {
    pub fn as_str(self) -> &'static str {
        match self {
            ResidentKey::Required => "required",
            ResidentKey::Preferred => "preferred",
            ResidentKey::Discouraged => "discouraged",
        }
    }
}

/// Which kind of authenticator the ceremony asks for.
///
/// Left unset by default. Asking for `Platform` hides every security key from
/// the dialog, and asking for `CrossPlatform` hides Touch ID — both are ways
/// to lock somebody out of their own account by narrowing a picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticatorAttachment {
    Platform,
    CrossPlatform,
}

impl AuthenticatorAttachment {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthenticatorAttachment::Platform => "platform",
            AuthenticatorAttachment::CrossPlatform => "cross-platform",
        }
    }
}

/// The user a credential is being registered to.
///
/// `id` is the *user handle*: the opaque bytes an authenticator stores inside
/// a discoverable credential and hands back on login. The specification is
/// explicit that it must not be an email address, a username, or anything else
/// that identifies the person — it is stored outside your control, on hardware
/// you do not own, and is readable by anyone holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEntity {
    id: Vec<u8>,
    name: String,
    display_name: String,
}

impl UserEntity {
    pub fn new(
        id: impl Into<Vec<u8>>,
        name: impl Into<String>,
        display_name: impl Into<String>,
    ) -> UserEntity {
        UserEntity { id: id.into(), name: name.into(), display_name: display_name.into() }
    }

    /// The user handle.
    pub fn id(&self) -> &[u8] {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub(crate) fn json(&self) -> Json {
        Json::object([
            ("id", Json::String(base64::encode_url(&self.id))),
            ("name", Json::String(self.name.clone())),
            ("displayName", Json::String(self.display_name.clone())),
        ])
    }
}

/// How long a ceremony may sit unfinished in the browser, in milliseconds.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// The site a credential belongs to, and the policy it is registered under.
///
/// Everything a ceremony is checked against lives here, which is why both
/// ceremonies are methods on it: there is exactly one place to look to answer
/// "what does this server accept".
///
/// The `id` is a domain — `example.com`, never a URL and never a port. The
/// origins are full origins (`https://example.com:8443`) and are compared as
/// whole strings, because that is the check phishing has to get past.
#[derive(Debug, Clone)]
pub struct RelyingParty {
    id: String,
    name: String,
    id_hash: [u8; 32],
    origins: Vec<String>,
    user_verification: UserVerification,
    resident_key: ResidentKey,
    attachment: Option<AuthenticatorAttachment>,
    timeout_ms: u64,
    allow_cross_origin: bool,
}

/// The host of a URL, without scheme, port or path — the shape an rp id has.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

impl RelyingParty {
    /// A relying party for `id`, expecting `https://{id}` and nothing else.
    ///
    /// The single origin is a deliberate default rather than an empty list: an
    /// empty list has to mean "refuse everything", and a framework that starts
    /// out refusing every login teaches people to widen the check until it
    /// stops complaining. If your site is on a port, or serves more than one
    /// origin, say so with [`RelyingParty::with_origins`].
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> RelyingParty {
        let id = id.into();
        let id_hash = sha256(id.as_bytes());
        let origins = vec![format!("https://{id}")];

        RelyingParty {
            id,
            name: name.into(),
            id_hash,
            origins,
            user_verification: UserVerification::default(),
            resident_key: ResidentKey::default(),
            attachment: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            allow_cross_origin: false,
        }
    }

    /// Build from `config/webauthn.json`, falling back to `app.name` and
    /// `app.url`, so a site that has configured nothing else still gets a
    /// relying party that matches itself.
    ///
    /// | Key                          | Meaning                                                   |
    /// |------------------------------|-----------------------------------------------------------|
    /// | `webauthn.id`                | The relying party id: a domain. Defaults to `app.url`'s host. |
    /// | `webauthn.name`              | Shown by the authenticator. Defaults to `app.name`.       |
    /// | `webauthn.origins`           | Expected origins, array or comma-separated. Defaults to `app.url`. |
    /// | `webauthn.user_verification` | `required`, `preferred` (default) or `discouraged`.       |
    /// | `webauthn.resident_key`      | `required`, `preferred` (default) or `discouraged`.       |
    /// | `webauthn.timeout_ms`        | How long the browser waits for the authenticator.         |
    ///
    /// Fails when no id can be found anywhere, because a relying party id is
    /// the one thing the specification will not let anyone guess.
    pub fn from_config(config: &Config) -> Result<RelyingParty> {
        let app_url = config.string("app.url", "");
        let id = match config.string("webauthn.id", "") {
            id if !id.is_empty() => id,
            _ => host_of(&app_url).ok_or_else(|| {
                Error::msg(
                    "WebAuthn needs a relying party id: set webauthn.id (a domain, such as \
                     example.com) or app.url so one can be taken from it.",
                )
            })?,
        };
        let name = match config.string("webauthn.name", "") {
            name if !name.is_empty() => name,
            _ => config.string("app.name", "Rustlavel"),
        };

        let mut party = RelyingParty::new(id, name);

        let origins = config.list("webauthn.origins");
        if !origins.is_empty() {
            party = party.with_origins(origins);
        } else if !app_url.is_empty() {
            party = party.with_origins([app_url.trim_end_matches('/').to_string()]);
        }

        match config.string("webauthn.user_verification", "").to_ascii_lowercase().as_str() {
            "required" => party = party.with_user_verification(UserVerification::Required),
            "discouraged" => party = party.with_user_verification(UserVerification::Discouraged),
            "" | "preferred" => {}
            other => return Err(Error::msg(format!("`{other}` is not a user verification policy; use required, preferred or discouraged"))),
        }
        match config.string("webauthn.resident_key", "").to_ascii_lowercase().as_str() {
            "required" => party = party.with_resident_key(ResidentKey::Required),
            "discouraged" => party = party.with_resident_key(ResidentKey::Discouraged),
            "" | "preferred" => {}
            other => return Err(Error::msg(format!("`{other}` is not a resident key policy; use required, preferred or discouraged"))),
        }

        let timeout_ms = config.int("webauthn.timeout_ms", 0);
        if timeout_ms > 0 {
            party = party.with_timeout_ms(timeout_ms as u64);
        }
        Ok(party)
    }

    /// Replace the expected origins. Every one is matched exactly.
    pub fn with_origins<S: Into<String>, I: IntoIterator<Item = S>>(
        mut self,
        origins: I,
    ) -> RelyingParty {
        self.origins = origins.into_iter().map(Into::into).collect();
        self
    }

    /// Add one more expected origin, keeping the ones already there.
    pub fn add_origin(mut self, origin: impl Into<String>) -> RelyingParty {
        self.origins.push(origin.into());
        self
    }

    pub fn with_user_verification(mut self, policy: UserVerification) -> RelyingParty {
        self.user_verification = policy;
        self
    }

    pub fn with_resident_key(mut self, policy: ResidentKey) -> RelyingParty {
        self.resident_key = policy;
        self
    }

    pub fn with_attachment(mut self, attachment: AuthenticatorAttachment) -> RelyingParty {
        self.attachment = Some(attachment);
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> RelyingParty {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Accept a ceremony run inside a cross-origin iframe.
    ///
    /// Off by default. `crossOrigin: true` means the page that called
    /// `navigator.credentials` was framed by a different site, which is the
    /// shape of an attacker embedding a real login and driving it. Turn it on
    /// only if you actually embed your own login somewhere and know where.
    pub fn allowing_cross_origin(mut self, allowed: bool) -> RelyingParty {
        self.allow_cross_origin = allowed;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// SHA-256 of the id, which is what an authenticator actually signs over.
    pub fn id_hash(&self) -> &[u8; 32] {
        &self.id_hash
    }

    pub fn origins(&self) -> &[String] {
        &self.origins
    }

    pub fn user_verification(&self) -> UserVerification {
        self.user_verification
    }

    pub fn resident_key(&self) -> ResidentKey {
        self.resident_key
    }

    pub fn attachment(&self) -> Option<AuthenticatorAttachment> {
        self.attachment
    }

    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    pub fn cross_origin_allowed(&self) -> bool {
        self.allow_cross_origin
    }

    /// The check phishing has to get past.
    ///
    /// Whole-string equality, and nothing else. No prefix match, because
    /// `https://example.com.evil.test` begins with `https://example.com`. No
    /// suffix match, because `https://notexample.com` ends with one too. No
    /// subdomain wildcard, because `https://anything.example.com` is a
    /// different origin and the browser already knows it — the whole value of
    /// this protocol is that the browser refuses to sign for the wrong one, so
    /// undoing that here would throw away the only property passwords lack.
    pub fn expect_origin(&self, origin: &str) -> Result<()> {
        if self.origins.is_empty() {
            return Err(Error::msg(
                "this relying party has no expected origin, so no origin can be accepted. An \
                 empty list cannot mean `anything`: it would accept a login collected by a \
                 copy of your page at some other address.",
            ));
        }

        if self.origins.iter().any(|expected| expected == origin) {
            return Ok(());
        }

        Err(Error::msg(format!(
            "the ceremony was run at origin `{origin}`, which is not one this server accepts \
             ({}). The comparison is exact: an origin that merely starts with, ends with or \
             sits under the right one is a different site.",
            self.origins.join(", ")
        )))
    }
}

/// The `clientDataJSON` the browser built, after checking it is well formed.
///
/// The browser writes this, not the authenticator, and the authenticator signs
/// a hash of it — so it is the part of the message that carries the origin,
/// and the part an attacker most wants to be able to rewrite.
#[derive(Clone)]
pub struct ClientData {
    kind: String,
    challenge: Vec<u8>,
    origin: String,
    cross_origin: bool,
}

impl ClientData {
    /// Decode and structurally check `clientDataJSON`.
    ///
    /// The challenge is decoded here rather than compared as text: a browser
    /// writes it unpadded base64url, but comparing strings would make padding
    /// or an alphabet difference look like a forgery, and comparing bytes
    /// cannot.
    pub fn parse(bytes: &[u8]) -> Result<ClientData> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::msg("clientDataJSON is not valid UTF-8"))?;
        let value = Json::parse(text)?;

        let kind = value
            .get("type")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::msg("clientDataJSON has no `type`"))?
            .to_string();
        let origin = value
            .get("origin")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::msg("clientDataJSON has no `origin`"))?
            .to_string();
        let challenge = base64_field(&value, "challenge")?;

        // Sixteen bytes is the specification's floor for a challenge, and a
        // short one is guessable — which turns a replay into an offline
        // exercise rather than a race.
        if challenge.len() < MINIMUM_CHALLENGE_BYTES {
            return Err(Error::msg(format!(
                "clientDataJSON carries a {}-byte challenge, and a challenge below {} bytes \
                 is guessable rather than unpredictable",
                challenge.len(),
                MINIMUM_CHALLENGE_BYTES
            )));
        }

        // Absent means same-origin: only a framed ceremony sets it.
        let cross_origin = value.get("crossOrigin").and_then(Json::as_bool).unwrap_or(false);

        Ok(ClientData { kind, challenge, origin, cross_origin })
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn challenge(&self) -> &[u8] {
        &self.challenge
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn is_cross_origin(&self) -> bool {
        self.cross_origin
    }

    /// Refuse a response from the other ceremony.
    ///
    /// Without this, an assertion collected by a site the user already logs
    /// into could be posted to a registration endpoint, or the reverse: the
    /// signature is genuine, the challenge could be genuine, and only the type
    /// says the message was never meant for this handler.
    pub fn expect_kind(&self, expected: &str) -> Result<()> {
        if self.kind == expected {
            return Ok(());
        }

        Err(Error::msg(format!(
            "clientDataJSON says this was a `{}` ceremony, and this endpoint finishes \
             `{expected}`. A response meant for the other ceremony is not evidence for this \
             one, however genuine its signature.",
            self.kind
        )))
    }

    /// Origin and framing, together, against a relying party.
    pub fn expect_origin(&self, rp: &RelyingParty) -> Result<()> {
        rp.expect_origin(&self.origin)?;

        if self.cross_origin && !rp.cross_origin_allowed() {
            return Err(Error::msg(
                "the ceremony ran inside a cross-origin iframe. The origin is right, but the \
                 page driving it belongs to somebody else — allow it explicitly if you really \
                 do embed your own login.",
            ));
        }

        Ok(())
    }
}

/// Origin and type, never the challenge.
impl std::fmt::Debug for ClientData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientData")
            .field("type", &self.kind)
            .field("origin", &self.origin)
            .field("crossOrigin", &self.cross_origin)
            .field("challenge", &format_args!("<{} bytes>", self.challenge.len()))
            .finish()
    }
}

/// The floor for a challenge, in bytes. From the specification.
pub const MINIMUM_CHALLENGE_BYTES: usize = 16;

/// The longest credential id an authenticator may produce, from CTAP2.
pub const MAX_CREDENTIAL_ID_BYTES: usize = 1023;

/// The fixed part of `authenticatorData`: hash, flags, counter.
const AUTHENTICATOR_DATA_HEADER: usize = 32 + 1 + 4;

const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_USER_VERIFIED: u8 = 0x04;
const FLAG_BACKUP_ELIGIBLE: u8 = 0x08;
const FLAG_BACKED_UP: u8 = 0x10;
const FLAG_ATTESTED_CREDENTIAL: u8 = 0x40;
const FLAG_EXTENSION_DATA: u8 = 0x80;

/// The flags byte of `authenticatorData`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatorFlags(u8);

impl AuthenticatorFlags {
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Somebody was there. Set by a touch, a button, a fingerprint.
    pub fn user_present(self) -> bool {
        self.0 & FLAG_USER_PRESENT != 0
    }

    /// The authenticator checked *who* was there — a PIN, a face, a print.
    pub fn user_verified(self) -> bool {
        self.0 & FLAG_USER_VERIFIED != 0
    }

    /// The credential may be copied to the user's other devices.
    pub fn backup_eligible(self) -> bool {
        self.0 & FLAG_BACKUP_ELIGIBLE != 0
    }

    /// The credential currently exists somewhere other than this device.
    pub fn backed_up(self) -> bool {
        self.0 & FLAG_BACKED_UP != 0
    }

    pub fn attested_credential_data(self) -> bool {
        self.0 & FLAG_ATTESTED_CREDENTIAL != 0
    }

    pub fn extension_data(self) -> bool {
        self.0 & FLAG_EXTENSION_DATA != 0
    }
}

/// The credential an authenticator produced during registration.
#[derive(Clone)]
pub struct AttestedCredential {
    aaguid: [u8; 16],
    id: Vec<u8>,
    key: CoseKey,
}

impl AttestedCredential {
    /// Which model of authenticator this is, or all zeroes when it declines
    /// to say — which is what a platform authenticator running a privacy-
    /// preserving `none` attestation does, and is not a fault.
    pub fn aaguid(&self) -> &[u8; 16] {
        &self.aaguid
    }

    pub fn id(&self) -> &[u8] {
        &self.id
    }

    pub fn key(&self) -> &CoseKey {
        &self.key
    }
}

impl std::fmt::Debug for AttestedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttestedCredential")
            .field("aaguid", &format_args!("{}", hex(&self.aaguid)))
            .field("id", &format_args!("<{} bytes>", self.id.len()))
            .field("key", &self.key)
            .finish()
    }
}

/// `authenticatorData`: what the authenticator itself asserts and signs.
#[derive(Debug, Clone)]
pub struct AuthenticatorData {
    rp_id_hash: [u8; 32],
    flags: AuthenticatorFlags,
    sign_count: u32,
    attested: Option<AttestedCredential>,
}

impl AuthenticatorData {
    /// Parse the structure, refusing anything left over.
    ///
    /// Trailing bytes matter here more than anywhere else in this package:
    /// `authenticatorData` is covered by the signature in full, so bytes that
    /// are signed but never parsed are bytes whose meaning the server and the
    /// authenticator disagree about. They are refused rather than ignored.
    pub fn parse(bytes: &[u8]) -> Result<AuthenticatorData> {
        if bytes.len() < AUTHENTICATOR_DATA_HEADER {
            return Err(Error::msg(format!(
                "authenticatorData is {} bytes and cannot be shorter than {}",
                bytes.len(),
                AUTHENTICATOR_DATA_HEADER
            )));
        }

        let mut rp_id_hash = [0u8; 32];
        rp_id_hash.copy_from_slice(&bytes[..32]);
        let flags = AuthenticatorFlags(bytes[32]);
        let sign_count = u32::from_be_bytes([bytes[33], bytes[34], bytes[35], bytes[36]]);

        // A credential cannot be backed up somewhere it was never eligible to
        // go. The combination is forbidden, so an authenticator claiming it is
        // either broken or telling a story about where the key lives.
        if flags.backed_up() && !flags.backup_eligible() {
            return Err(Error::msg(
                "authenticatorData says the credential is backed up but not eligible for \
                 backup, which the specification forbids",
            ));
        }

        let mut rest = &bytes[AUTHENTICATOR_DATA_HEADER..];

        let attested = if flags.attested_credential_data() {
            let (credential, consumed) = parse_attested_credential(rest)?;
            rest = &rest[consumed..];
            Some(credential)
        } else {
            None
        };

        if flags.extension_data() {
            // Nothing here reads extensions, but they have to be consumed or
            // the length check below cannot tell them from junk.
            let (_, consumed) = Cbor::parse_prefix(rest)?;
            rest = &rest[consumed..];
        }

        if !rest.is_empty() {
            return Err(Error::msg(format!(
                "authenticatorData has {} bytes after everything its flags declare. They are \
                 covered by the signature and understood by nobody, so they are refused.",
                rest.len()
            )));
        }

        Ok(AuthenticatorData { rp_id_hash, flags, sign_count, attested })
    }

    pub fn rp_id_hash(&self) -> &[u8; 32] {
        &self.rp_id_hash
    }

    pub fn flags(&self) -> AuthenticatorFlags {
        self.flags
    }

    pub fn sign_count(&self) -> u32 {
        self.sign_count
    }

    pub fn attested_credential(&self) -> Option<&AttestedCredential> {
        self.attested.as_ref()
    }

    /// The credential is for this site and no other.
    pub fn expect_rp_id(&self, rp: &RelyingParty) -> Result<()> {
        if &self.rp_id_hash == rp.id_hash() {
            return Ok(());
        }

        Err(Error::msg(format!(
            "the authenticator signed for a different relying party: the data carries the \
             SHA-256 of some other id, not of `{}`. A credential registered to one site must \
             not be usable at another.",
            rp.id()
        )))
    }

    /// Somebody was actually there.
    pub fn expect_user_present(&self) -> Result<()> {
        if self.flags.user_present() {
            return Ok(());
        }

        Err(Error::msg(
            "the User Present flag is not set: the authenticator produced this without anybody \
             touching it. A credential exercised silently is what malware on the device looks \
             like, not what a person logging in looks like.",
        ))
    }

    /// And, if the policy says so, the authenticator checked who.
    pub fn expect_user_verification(&self, policy: UserVerification) -> Result<()> {
        if !policy.is_required() || self.flags.user_verified() {
            return Ok(());
        }

        Err(Error::msg(
            "user verification was required and the authenticator did not perform it. The \
             signature proves the device is present; it does not prove the person holding it \
             identified themselves to it.",
        ))
    }
}

/// aaguid || credentialIdLength || credentialId || credentialPublicKey.
fn parse_attested_credential(bytes: &[u8]) -> Result<(AttestedCredential, usize)> {
    const HEADER: usize = 16 + 2;

    if bytes.len() < HEADER {
        return Err(Error::msg(
            "authenticatorData claims attested credential data and then ends before it",
        ));
    }

    let mut aaguid = [0u8; 16];
    aaguid.copy_from_slice(&bytes[..16]);

    let id_length = u16::from_be_bytes([bytes[16], bytes[17]]) as usize;
    if id_length == 0 {
        return Err(Error::msg("the attested credential has an empty credential id"));
    }
    if id_length > MAX_CREDENTIAL_ID_BYTES {
        return Err(Error::msg(format!(
            "the attested credential id claims {id_length} bytes, and CTAP2 caps it at \
             {MAX_CREDENTIAL_ID_BYTES}"
        )));
    }

    let end = HEADER + id_length;
    if bytes.len() < end {
        return Err(Error::msg(format!(
            "the attested credential id claims {id_length} bytes and only {} remain",
            bytes.len() - HEADER
        )));
    }

    let id = bytes[HEADER..end].to_vec();
    let (value, consumed) = Cbor::parse_prefix(&bytes[end..])?;
    let key = CoseKey::parse(&value)?;

    Ok((AttestedCredential { aaguid, id, key }, end + consumed))
}

/// Lowercase hex, for an AAGUID in a debug line.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// A software authenticator, so the ceremony tests can sign for real.
///
/// Nothing here is a mock: it builds the same bytes a YubiKey or a phone
/// builds and signs them with a P-256 key, so a passing test is an end-to-end
/// round trip. What it adds over a captured fixture is the ability to corrupt
/// exactly one field and leave the rest genuine, which is the only way to know
/// that a check is the one doing the refusing.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use p256::ecdsa::signature::Signer;

    pub(crate) struct Authenticator {
        signing: p256::ecdsa::SigningKey,
        pub(crate) aaguid: [u8; 16],
        pub(crate) credential_id: Vec<u8>,
    }

    impl Authenticator {
        /// A deterministic authenticator. `seed` picks the key and the id, so
        /// two of them in one test are two different devices.
        pub(crate) fn new(seed: u8) -> Authenticator {
            Authenticator {
                signing: p256::ecdsa::SigningKey::from_bytes(&[seed; 32].into())
                    .expect("a valid P-256 scalar"),
                aaguid: [seed; 16],
                credential_id: vec![seed; 20],
            }
        }

        pub(crate) fn with_credential_id(mut self, id: Vec<u8>) -> Authenticator {
            self.credential_id = id;
            self
        }

        /// `{1: 2, 3: -7, -1: 1, -2: x, -3: y}`.
        pub(crate) fn cose_key(&self) -> Vec<u8> {
            let point = self.signing.verifying_key().to_encoded_point(false);
            let mut out = Vec::new();
            map(5, &mut out);
            int(1, &mut out);
            int(2, &mut out);
            int(3, &mut out);
            int(-7, &mut out);
            int(-1, &mut out);
            int(1, &mut out);
            int(-2, &mut out);
            bytes(point.x().expect("an x coordinate"), &mut out);
            int(-3, &mut out);
            bytes(point.y().expect("a y coordinate"), &mut out);
            out
        }

        /// `rpIdHash || flags || signCount [ || attested credential data ]`.
        pub(crate) fn authenticator_data(
            &self,
            rp_id: &str,
            flags: u8,
            sign_count: u32,
        ) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&sha256(rp_id.as_bytes()));
            out.push(flags);
            out.extend_from_slice(&sign_count.to_be_bytes());

            if flags & FLAG_ATTESTED_CREDENTIAL != 0 {
                out.extend_from_slice(&self.aaguid);
                out.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
                out.extend_from_slice(&self.credential_id);
                out.extend_from_slice(&self.cose_key());
            }

            out
        }

        /// `{"fmt": .., "attStmt": {..}, "authData": ..}`, in canonical order.
        pub(crate) fn attestation_object(
            &self,
            format: &str,
            statement_entries: u64,
            auth_data: &[u8],
        ) -> Vec<u8> {
            let mut out = Vec::new();
            map(3, &mut out);
            text("fmt", &mut out);
            text(format, &mut out);
            text("attStmt", &mut out);
            map(statement_entries, &mut out);
            for index in 0..statement_entries {
                text(&format!("x{index}"), &mut out);
                int(index as i64, &mut out);
            }
            text("authData", &mut out);
            bytes(auth_data, &mut out);
            out
        }

        pub(crate) fn client_data(&self, kind: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
            format!(
                "{{\"type\":\"{kind}\",\"challenge\":\"{}\",\"origin\":\"{origin}\",\
                 \"crossOrigin\":false}}",
                base64::encode_url(challenge)
            )
            .into_bytes()
        }

        /// `authenticatorData || SHA-256(clientDataJSON)`, in DER.
        pub(crate) fn sign(&self, auth_data: &[u8], client_data: &[u8]) -> Vec<u8> {
            let mut message = auth_data.to_vec();
            message.extend_from_slice(&sha256(client_data));

            let signature: p256::ecdsa::Signature = self.signing.sign(&message);
            signature.to_der().as_bytes().to_vec()
        }
    }

    /// Just enough of a CBOR writer to build what an authenticator sends.
    fn head(major: u8, value: u64, out: &mut Vec<u8>) {
        let major = major << 5;
        match value {
            0..=23 => out.push(major | value as u8),
            24..=0xff => {
                out.push(major | 24);
                out.push(value as u8);
            }
            0x100..=0xffff => {
                out.push(major | 25);
                out.extend_from_slice(&(value as u16).to_be_bytes());
            }
            _ => {
                out.push(major | 26);
                out.extend_from_slice(&(value as u32).to_be_bytes());
            }
        }
    }

    pub(crate) fn map(entries: u64, out: &mut Vec<u8>) {
        head(5, entries, out);
    }

    pub(crate) fn text(value: &str, out: &mut Vec<u8>) {
        head(3, value.len() as u64, out);
        out.extend_from_slice(value.as_bytes());
    }

    pub(crate) fn bytes(value: &[u8], out: &mut Vec<u8>) {
        head(2, value.len() as u64, out);
        out.extend_from_slice(value);
    }

    pub(crate) fn int(value: i64, out: &mut Vec<u8>) {
        if value >= 0 {
            head(0, value as u64, out);
        } else {
            head(1, (-1 - value) as u64, out);
        }
    }

    /// The flags a healthy passkey registration carries.
    pub(crate) const REGISTRATION_FLAGS: u8 =
        FLAG_USER_PRESENT | FLAG_USER_VERIFIED | FLAG_ATTESTED_CREDENTIAL;

    /// And a healthy assertion.
    pub(crate) const ASSERTION_FLAGS: u8 = FLAG_USER_PRESENT | FLAG_USER_VERIFIED;
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    fn relying_party() -> RelyingParty {
        RelyingParty::new("example.com", "Example")
    }

    #[test]
    fn an_origin_must_match_exactly_and_not_by_prefix() {
        let rp = relying_party();

        assert!(rp.expect_origin("https://example.com").is_ok());

        // Every one of these starts, ends or sits under the right origin.
        for wrong in [
            "https://example.com.evil.test",
            "https://example.com/",
            "https://example.co",
            "http://example.com",
            "https://login.example.com",
            "https://example.com:8443",
            "https://EXAMPLE.com",
        ] {
            let error = rp.expect_origin(wrong).unwrap_err().to_string();
            assert!(error.contains("exact"), "{wrong} was accepted or misreported: {error}");
        }
    }

    #[test]
    fn a_relying_party_with_no_origins_refuses_everything() {
        // The alternative — an empty list meaning "anything" — accepts a login
        // collected by a copy of the page at another address.
        let rp = relying_party().with_origins(Vec::<String>::new());

        let error = rp.expect_origin("https://example.com").unwrap_err().to_string();
        assert!(error.contains("no expected origin"), "got {error}");
    }

    #[test]
    fn client_data_reads_the_fields_the_checks_depend_on() {
        let device = Authenticator::new(1);
        let bytes = device.client_data("webauthn.create", &[7u8; 32], "https://example.com");
        let data = ClientData::parse(&bytes).unwrap();

        assert_eq!(data.kind(), "webauthn.create");
        assert_eq!(data.challenge(), &[7u8; 32]);
        assert_eq!(data.origin(), "https://example.com");
        assert!(!data.is_cross_origin());
        assert!(data.expect_kind("webauthn.create").is_ok());
    }

    #[test]
    fn a_response_from_the_other_ceremony_is_refused_by_type() {
        let device = Authenticator::new(1);
        let bytes = device.client_data("webauthn.get", &[7u8; 32], "https://example.com");
        let data = ClientData::parse(&bytes).unwrap();

        let error = data.expect_kind("webauthn.create").unwrap_err().to_string();
        assert!(error.contains("webauthn.get"), "got {error}");
    }

    #[test]
    fn a_cross_origin_ceremony_is_refused_unless_it_was_allowed() {
        let json = br#"{"type":"webauthn.get","challenge":"AAAAAAAAAAAAAAAAAAAAAA",
                        "origin":"https://example.com","crossOrigin":true}"#;
        let data = ClientData::parse(json).unwrap();

        assert!(data.is_cross_origin());

        let error = data.expect_origin(&relying_party()).unwrap_err().to_string();
        assert!(error.contains("iframe"), "got {error}");

        assert!(data.expect_origin(&relying_party().allowing_cross_origin(true)).is_ok());
    }

    #[test]
    fn a_short_challenge_in_client_data_is_refused() {
        // Eight bytes is guessable, which turns a replay into an offline
        // exercise rather than a race.
        let json = br#"{"type":"webauthn.get","challenge":"AAAAAAAAAAA",
                        "origin":"https://example.com"}"#;

        let error = ClientData::parse(json).unwrap_err().to_string();
        assert!(error.contains("guessable"), "got {error}");
    }

    #[test]
    fn malformed_client_data_is_an_error_and_never_a_panic() {
        for input in [
            b"".as_slice(),
            b"{",
            b"null",
            b"{}",
            br#"{"type":"webauthn.get"}"#,
            br#"{"type":"webauthn.get","challenge":"!!!!","origin":"https://example.com"}"#,
            &[0xff, 0xfe],
        ] {
            assert!(ClientData::parse(input).is_err(), "{input:?} was accepted");
        }
    }

    #[test]
    fn authenticator_data_reads_the_hash_flags_counter_and_credential() {
        let device = Authenticator::new(3);
        let bytes = device.authenticator_data("example.com", REGISTRATION_FLAGS, 42);
        let data = AuthenticatorData::parse(&bytes).unwrap();

        assert_eq!(data.rp_id_hash(), relying_party().id_hash());
        assert_eq!(data.sign_count(), 42);
        assert!(data.flags().user_present());
        assert!(data.flags().user_verified());

        let attested = data.attested_credential().expect("the AT flag was set");
        assert_eq!(attested.id(), device.credential_id.as_slice());
        assert_eq!(attested.aaguid(), &[3u8; 16]);
        assert_eq!(attested.key().algorithm(), crate::SignatureAlgorithm::Es256);
    }

    #[test]
    fn a_wrong_relying_party_hash_is_refused() {
        let device = Authenticator::new(3);
        let bytes = device.authenticator_data("evil.test", REGISTRATION_FLAGS, 1);
        let data = AuthenticatorData::parse(&bytes).unwrap();

        let error = data.expect_rp_id(&relying_party()).unwrap_err().to_string();
        assert!(error.contains("different relying party"), "got {error}");
    }

    #[test]
    fn user_present_and_user_verified_are_checked_separately() {
        let device = Authenticator::new(3);

        let none = AuthenticatorData::parse(&device.authenticator_data("example.com", 0, 1))
            .unwrap();
        assert!(none.expect_user_present().unwrap_err().to_string().contains("User Present"));

        let present =
            AuthenticatorData::parse(&device.authenticator_data("example.com", 0x01, 1)).unwrap();
        assert!(present.expect_user_present().is_ok());
        assert!(present.expect_user_verification(UserVerification::Preferred).is_ok());

        let error = present
            .expect_user_verification(UserVerification::Required)
            .unwrap_err()
            .to_string();
        assert!(error.contains("user verification was required"), "got {error}");
    }

    #[test]
    fn bytes_after_the_declared_structure_are_refused_not_ignored() {
        // They are covered by the signature and understood by nobody, which is
        // exactly where a server and an authenticator can be made to disagree.
        let device = Authenticator::new(3);
        let mut bytes = device.authenticator_data("example.com", REGISTRATION_FLAGS, 1);
        bytes.push(0x00);

        let error = AuthenticatorData::parse(&bytes).unwrap_err().to_string();
        assert!(error.contains("after everything its flags declare"), "got {error}");
    }

    #[test]
    fn truncated_authenticator_data_is_an_error_and_never_a_panic() {
        let device = Authenticator::new(3);
        let complete = device.authenticator_data("example.com", REGISTRATION_FLAGS, 1);

        for length in 0..complete.len() {
            assert!(
                AuthenticatorData::parse(&complete[..length]).is_err(),
                "prefix of {length} bytes was accepted"
            );
        }
        assert!(AuthenticatorData::parse(&complete).is_ok());
    }

    #[test]
    fn a_credential_id_longer_than_the_bytes_that_follow_is_refused() {
        let device = Authenticator::new(3);
        let mut bytes = device.authenticator_data("example.com", REGISTRATION_FLAGS, 1);
        // The length field sits at 37 + 16.
        bytes[53] = 0xff;
        bytes[54] = 0xff;

        let error = AuthenticatorData::parse(&bytes).unwrap_err().to_string();
        assert!(error.contains("caps it at"), "got {error}");
    }

    #[test]
    fn backed_up_without_backup_eligible_is_refused() {
        let device = Authenticator::new(3);
        let bytes = device.authenticator_data("example.com", 0x01 | 0x10, 1);

        let error = AuthenticatorData::parse(&bytes).unwrap_err().to_string();
        assert!(error.contains("forbids"), "got {error}");
    }

    #[test]
    fn debug_output_names_nothing_that_identifies_a_person() {
        let device = Authenticator::new(3);
        let data =
            AuthenticatorData::parse(&device.authenticator_data("example.com", 0x41, 1)).unwrap();
        let printed = format!("{data:?}");

        assert!(printed.contains("aaguid"), "got {printed}");
        assert!(printed.contains("<20 bytes>"), "the credential id was printed: {printed}");
        assert!(printed.contains("<public key>"), "the key was printed: {printed}");

        let client = ClientData::parse(&device.client_data(
            "webauthn.get",
            &[9u8; 32],
            "https://example.com",
        ))
        .unwrap();
        let printed = format!("{client:?}");
        assert!(printed.contains("<32 bytes>"), "the challenge was printed: {printed}");
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn from_config_falls_back_to_the_applications_own_name_and_url() {
        let config = Config::new();
        config.set("app.name", "Acme");
        config.set("app.url", "https://app.acme.example:8443/");

        let party = RelyingParty::from_config(&config).expect("app.url is enough");
        assert_eq!(party.id(), "app.acme.example", "the host, without scheme or port");
        assert_eq!(party.name, "Acme");
        assert_eq!(party.origins, vec!["https://app.acme.example:8443"], "the origin keeps its port");
    }

    #[test]
    fn from_config_reads_every_key_and_a_comma_separated_origin_list() {
        let config = Config::new();
        config.set("webauthn.id", "acme.example");
        config.set("webauthn.name", "Acme Passkeys");
        config.set("webauthn.origins", "https://acme.example, https://www.acme.example");
        config.set("webauthn.user_verification", "required");
        config.set("webauthn.resident_key", "discouraged");
        config.set("webauthn.timeout_ms", Json::from(30_000_i64));

        let party = RelyingParty::from_config(&config).unwrap();
        assert_eq!(party.id(), "acme.example");
        assert_eq!(party.name, "Acme Passkeys");
        assert_eq!(party.origins.len(), 2);
        assert_eq!(party.user_verification, UserVerification::Required);
        assert_eq!(party.resident_key, ResidentKey::Discouraged);
        assert_eq!(party.timeout_ms, 30_000);
    }

    #[test]
    fn from_config_refuses_to_guess_an_id_and_a_misspelt_policy() {
        let error = RelyingParty::from_config(&Config::new()).expect_err("no id anywhere").to_string();
        assert!(error.contains("webauthn.id"), "{error}");

        let config = Config::new();
        config.set("webauthn.id", "acme.example");
        config.set("webauthn.user_verification", "always");
        let error = RelyingParty::from_config(&config).err().unwrap().to_string();
        assert!(error.contains("`always`"), "{error}");
    }

    #[test]
    fn host_of_handles_the_shapes_a_url_takes() {
        assert_eq!(host_of("https://acme.example"), Some("acme.example".into()));
        assert_eq!(host_of("https://acme.example:8443/login?x=1"), Some("acme.example".into()));
        assert_eq!(host_of("http://user:pw@acme.example/"), Some("acme.example".into()));
        assert_eq!(host_of("acme.example"), Some("acme.example".into()));
        assert_eq!(host_of(""), None);
    }
}
